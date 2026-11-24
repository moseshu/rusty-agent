// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! The coding product's per-thread collaboration version, per-model delegation gate, and the
//! collaboration tool table that `[features.multi_agent_v2]` configures.

use std::{
    collections::BTreeSet,
    sync::{Arc, OnceLock},
};

use async_trait::async_trait;
use ra_core::{
    context::RunContext,
    error::{Error, Result},
    session::SessionId,
    tool::{
        DecodedToolInput, FuncSchema, Tool, ToolContext, ToolNamespace, ToolOptions, ToolOrigin,
        ToolOutput, ToolResolver, ToolSchema, ToolServices,
    },
};
use ra_protocol::server::{SessionSource, SubAgentSource, ThreadFactoryRequest};
use ra_runtime::runner::RunConfig;
use ra_tools::agent_ns::{
    CollaborationKind, CollaborationTool, MULTI_AGENT_V1_NAMESPACE, WaitAgentTimeoutOptions,
};

use crate::config::{MultiAgentVersion, model_runtime::ProductModelRuntime};

mod mode;
mod role;

use role::{Role, ThreadModelContext};

pub(crate) const COLLABORATION_KINDS: [CollaborationKind; 6] = [
    CollaborationKind::SpawnAgent,
    CollaborationKind::SendMessage,
    CollaborationKind::FollowupTask,
    CollaborationKind::InterruptAgent,
    CollaborationKind::WaitAgent,
    CollaborationKind::ListAgents,
];

const COLLABORATION_V1_KINDS: [CollaborationKind; 5] = [
    CollaborationKind::SpawnAgentV1,
    CollaborationKind::SendInput,
    CollaborationKind::WaitAgentV1,
    CollaborationKind::ResumeAgent,
    CollaborationKind::CloseAgent,
];

pub(crate) struct ThreadCollaboration {
    runtime: Arc<ProductModelRuntime>,
    version: OnceLock<MultiAgentVersion>,
    /// A recorded value this build does not recognize. It is kept as recorded rather than replaced
    /// by a migrated value, and the thread is offered no collaboration tools.
    unrecognized: Option<String>,
    spawned: bool,
    spawn_depth: i32,
    session_id: SessionId,
    role: Option<Role>,
    tools: ToolTable,
}

/// What `[features.multi_agent_v2]` makes of the V2 tool table, as Codex's spec plan reads it.
struct ToolTable {
    /// `tool_namespace`; validated by the configuration loader.
    namespace: Option<String>,
    /// `disable_native_tools`: no collaboration tools at all.
    native_tools: bool,
    /// `disable_direct_message`: without it, `send_message` and `followup_task` are not offered.
    direct_message: bool,
    /// `wait_agent_enabled`.
    wait_agent: bool,
    wait: WaitAgentTimeoutOptions,
    /// `usage_hint_text`, appended to the `spawn_agent` description.
    usage_hint: Option<String>,
    /// `max_concurrent_threads_per_session` less the root: Codex's `effective_agent_max_threads`.
    max_spawned_threads: usize,
}

impl ToolTable {
    fn new(runtime: &ProductModelRuntime) -> Self {
        let config = runtime.multi_agent_v2();
        Self {
            namespace: config.tool_namespace.clone(),
            native_tools: !config.disable_native_tools,
            direct_message: !config.disable_direct_message,
            wait_agent: config.wait_agent_enabled,
            wait: WaitAgentTimeoutOptions::new().with_bounds(
                config.default_wait_timeout_ms,
                config.min_wait_timeout_ms,
                config.max_wait_timeout_ms,
            ),
            usage_hint: config.usage_hint_text.clone(),
            max_spawned_threads: config.max_concurrent_threads_per_session.saturating_sub(1),
        }
    }
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
        let tools = ToolTable::new(&runtime);
        Self {
            runtime,
            tools,
            version: version.map(OnceLock::from).unwrap_or_default(),
            unrecognized,
            session_id: request.thread_id().clone(),
            role: match request.session_source() {
                SessionSource::SubAgent(SubAgentSource::ThreadSpawn { .. }) => Some(Role::Subagent),
                SessionSource::Cli
                | SessionSource::VSCode
                | SessionSource::Exec
                | SessionSource::Mcp
                | SessionSource::Custom(_)
                | SessionSource::Unknown => Some(if request.thread_spawn().is_some() {
                    Role::Subagent
                } else {
                    Role::Root
                }),
                _ => None,
            },
            spawn_depth: match request.session_source() {
                SessionSource::SubAgent(SubAgentSource::ThreadSpawn { depth, .. }) => *depth,
                _ => request
                    .thread_spawn()
                    .map_or(0, |spawn| i32::try_from(spawn.depth()).unwrap_or(i32::MAX)),
            },
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

    /// Whether the static inventory reserves entries for V2 per-turn selection.
    pub(crate) fn can_offer_tools(&self) -> bool {
        self.can_register_version(MultiAgentVersion::V2)
    }

    fn can_register_version(&self, version: MultiAgentVersion) -> bool {
        (version != MultiAgentVersion::V2 || self.tools.native_tools)
            && self
                .version
                .get()
                .is_none_or(|selected| *selected == version)
    }

    pub(crate) fn is_v1(&self) -> bool {
        self.version.get() == Some(&MultiAgentVersion::V1)
    }

    pub(crate) fn v1_tools(
        self: &Arc<Self>,
        spawned: bool,
        namespaced: bool,
    ) -> Result<Vec<Arc<dyn Tool>>> {
        if !self.can_register_version(MultiAgentVersion::V1) {
            return Ok(Vec::new());
        }
        let namespace = namespaced
            .then(|| ToolNamespace::new(MULTI_AGENT_V1_NAMESPACE))
            .transpose()?;
        COLLABORATION_V1_KINDS
            .into_iter()
            .map(|kind| {
                let tool = CollaborationTool::new(kind, WaitAgentTimeoutOptions::new())?;
                let tool = match &namespace {
                    Some(namespace) => tool.in_namespace(namespace.clone())?,
                    None => tool,
                };
                let tool = if kind == CollaborationKind::SpawnAgentV1 {
                    tool.with_v1_spawn_description(
                        &self.runtime.v1_spawn_models_description(),
                        self.runtime.v1_spawn_roles_description().as_deref(),
                        self.tools.usage_hint.clone(),
                    )?
                } else {
                    tool
                };
                Ok(self.tool(tool, spawned))
            })
            .collect()
    }

    pub(crate) fn runtime(&self) -> &ProductModelRuntime {
        &self.runtime
    }

    pub(crate) fn v1_limits(&self) -> (usize, i32) {
        self.runtime.v1_agent_limits()
    }

    /// The configured namespace, before any explicit host choice.
    pub(crate) fn tool_namespace(&self) -> Option<&str> {
        self.tools.namespace.as_deref()
    }

    /// The configured `wait_agent` timeouts.
    pub(crate) const fn wait_timeouts(&self) -> WaitAgentTimeoutOptions {
        self.tools.wait
    }

    /// Whether the configured tool table offers `kind`.
    pub(crate) fn offers(&self, kind: CollaborationKind) -> bool {
        self.can_register_version(MultiAgentVersion::V2)
            && match kind {
                CollaborationKind::SendMessage | CollaborationKind::FollowupTask => {
                    self.tools.direct_message
                }
                CollaborationKind::WaitAgent => self.tools.wait_agent,
                _ => true,
            }
    }

    /// How many spawned agents the thread's tree runs at once.
    pub(crate) const fn max_spawned_threads(&self) -> usize {
        self.tools.max_spawned_threads
    }

    pub(crate) fn install(
        self: &Arc<Self>,
        config: RunConfig,
        namespace: Option<ToolNamespace>,
        collaborates: bool,
    ) -> RunConfig {
        let thread = Arc::clone(self);
        config
            .with_context_filter(Arc::new(ThreadModelContext {
                thread: Arc::clone(self),
                namespace,
                collaborates,
            }))
            .with_turn_context_enricher(move |_, record| {
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

    fn version_for_model(&self, model_version: Option<MultiAgentVersion>) -> MultiAgentVersion {
        self.version
            .get()
            .copied()
            .unwrap_or_else(|| self.runtime.multi_agent_version_for_model(model_version))
    }

    /// Codex gates V2 delegation separately from the session's role instructions.
    fn tools_enabled_for_model(
        &self,
        model_version: Option<MultiAgentVersion>,
        spawned_in_tree: bool,
    ) -> bool {
        self.tools.native_tools
            && self.version_for_model(model_version) == MultiAgentVersion::V2
            && (!(spawned_in_tree || self.spawned) || model_version == Some(MultiAgentVersion::V2))
    }

    pub(crate) fn tool(
        self: &Arc<Self>,
        tool: CollaborationTool,
        spawned_in_tree: bool,
    ) -> Arc<dyn Tool> {
        // Codex appends the configured usage hint to the composed `spawn_agent` description.
        let schema = match (&self.tools.usage_hint, tool.kind()) {
            (Some(hint), CollaborationKind::SpawnAgent) => {
                let description = tool.schema().description().unwrap_or_default();
                tool.schema()
                    .clone()
                    .with_description(format!("{description}\n{hint}"))
            }
            _ => tool.schema().clone(),
        };
        Arc::new(ProductCollaborationTool {
            tool,
            schema,
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

pub(crate) struct LegacyCollaborationResolver {
    pub(crate) tools: Vec<Arc<dyn Tool>>,
    pub(crate) external: Option<Arc<dyn ToolResolver>>,
}

#[async_trait]
impl ToolResolver for LegacyCollaborationResolver {
    fn requires_reserved_tool_names(&self) -> bool {
        self.external
            .as_ref()
            .is_some_and(|resolver| resolver.requires_reserved_tool_names())
    }

    async fn get_tools(
        &self,
        context: &RunContext,
        reserved: &BTreeSet<String>,
    ) -> Result<Vec<Arc<dyn Tool>>> {
        let mut tools = Vec::new();
        for tool in &self.tools {
            if tool.is_enabled(context).await? {
                tools.push(Arc::clone(tool));
            }
        }
        if let Some(external) = &self.external {
            let mut reserved = reserved.clone();
            reserved.extend(
                tools
                    .iter()
                    .map(|tool| tool.model_definition().qualified_name()),
            );
            tools.extend(external.get_tools(context, &reserved).await?);
        }
        Ok(tools)
    }
}

struct ProductCollaborationTool {
    tool: CollaborationTool,
    schema: ToolSchema,
    thread: Arc<ThreadCollaboration>,
    spawned_in_tree: bool,
}

#[async_trait]
impl Tool for ProductCollaborationTool {
    fn origin(&self) -> &ToolOrigin {
        self.tool.origin()
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
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
        let model_version =
            if self.spawned_in_tree || self.thread.spawned || self.thread.version.get().is_none() {
                self.thread.model_version(context.resolved_model_name())
            } else {
                None
            };
        if self.tool.kind().is_v1() {
            return Ok(self.thread.version_for_model(model_version) == MultiAgentVersion::V1);
        }
        Ok(self
            .thread
            .tools_enabled_for_model(model_version, self.spawned_in_tree))
    }

    async fn is_enabled_with_services(
        &self,
        context: &RunContext,
        services: &ToolServices,
    ) -> Result<bool> {
        let Some(port) = services.agent_control() else {
            return Ok(false);
        };
        if self.tool.kind().is_v1() {
            // For the first request there is no locked version yet. Check the V1 limit while
            // preparing the table without committing the version of an unsampled request.
            let next_local_depth =
                i32::try_from(port.caller().as_str().split('/').count() - 1).unwrap_or(i32::MAX);
            if port.spawn_depth_exceeded()
                || self.thread.spawn_depth.saturating_add(next_local_depth)
                    > self.thread.v1_limits().1
            {
                return Ok(false);
            }
        }
        self.is_enabled(context).await
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
