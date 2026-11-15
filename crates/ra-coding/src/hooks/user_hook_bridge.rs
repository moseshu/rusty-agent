// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Product hooks behind the runtime's host hook dispatcher.
//!
//! The source core builds each hook request from its session and turn state (`hook_runtime.rs`)
//! and applies the outcome itself. Here the runtime owns dispatch, so each supported event is
//! registered as a [`UserHook`] that builds the same request and returns the control effect the
//! dispatcher can apply: a `PreToolUse` block becomes a denial with the source's message, a
//! `PermissionRequest` decision becomes a grant or denial, and a Stop or `SubagentStop` block becomes
//! a continuation prompt.
//!
//! Context is delivered through the run-owned history queue; native argument rewrites and
//! model-visible result replacements are applied at the runtime admission boundaries.
//! Session stop-processing is returned as a host-owned prompt rejection.
//!
//! Each run is reported as the source reports it: the previewed runs as `hook/started` before
//! they run and the finished ones as `hook/completed`, built-in and background runs left out, on
//! the thread's ordered event stream once [`ProductHookRuntime::bind_thread_events`] connects it.
//! A background handler that finishes later delivers its context and warnings to the current turn
//! instead. Refreshing the configuration keeps running background handlers, as the source's
//! reconfiguration does.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, RwLock};

use async_trait::async_trait;
use ra_config::AskForApproval;
use ra_core::error::Result;
use ra_core::hook::HookContextQueue;
use ra_core::hook::{
    CompactTrigger, HookDecision, HookEvent, HookEventName, ToolHookData, UserHook, UserHookContext,
};
use ra_core::item::{Message, MessageRole, ModelInputItem};
use ra_core::session::SessionId;
use ra_core::session::rollout::{PersistContext, RolloutRecorder};
use ra_paths::AbsolutePathBuf;
use ra_protocol::server::protocol::{
    HOOK_COMPLETED_METHOD, HOOK_STARTED_METHOD, HookCompletedEvent, HookExecutionMode,
    HookOutputEntryKind, HookRunSummary, HookStartedEvent,
};
use ra_runtime::hook::{UserHookRegistration, UserHooks};
use ra_runtime::thread::{ThreadEvent, ThreadNotice};
use serde_json::Value;
use tokio::sync::mpsc;

use super::events::compact::{PostCompactRequest, PreCompactRequest};
use super::events::interrupt::InterruptRequest;
use super::events::permission_request::{PermissionRequestDecision, PermissionRequestRequest};
use super::events::post_tool_use::PostToolUseRequest;
use super::events::pre_tool_use::PreToolUseRequest;
use super::events::session_end::SessionEndRequest;
use super::events::session_start::{SessionStartRequest, StartHookTarget};
use super::events::stop::{StopHookTarget, StopRequest};
use super::events::user_prompt_submit::UserPromptSubmitRequest;
use super::types::{HookEventAfterAgent, HookPayload, HookResult};
use super::{Hooks, HooksConfig};

/// Session identity and fallback request values for hosts without runtime metadata.
#[derive(Debug, Clone)]
pub struct ProductHookSession {
    /// Thread identity serialized as `session_id`.
    pub session_id: SessionId,
    /// Host-side working directory; hooks always run on the host.
    pub cwd: AbsolutePathBuf,
    /// Fallback model slug when the run supplies no resolved model.
    pub model: String,
    /// Fallback approval policy, mapped to the source's `permission_mode` labels.
    pub approval_policy: AskForApproval,
    /// Fallback transcript for a host without a late-bound thread source.
    pub transcript_path: Option<PathBuf>,
}

/// A hook result the runtime dispatcher cannot apply.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum UnappliedHookEffect {
    /// Text the source would add to the model context.
    AdditionalContext {
        /// Event that produced it.
        event: HookEventName,
        /// The context fragments, in handler order.
        contexts: Vec<String>,
    },
    /// Replacement arguments for a `PreToolUse` call.
    UpdatedInput {
        /// Call the hook rewrote.
        tool_use_id: String,
        /// The rewritten arguments.
        input: Value,
    },
    /// A request to stop processing the turn or session.
    StopProcessing {
        /// Event that produced it.
        event: HookEventName,
        /// The hook's reason, when given.
        reason: Option<String>,
    },
    /// Model-facing feedback or a block after a tool already ran.
    PostToolUseFeedback {
        /// Call the feedback concerns.
        tool_use_id: String,
        /// The feedback message, when given.
        message: Option<String>,
    },
}

/// Executor plugin hook sources for the step a run executes: Codex's
/// `executor_hook_sources_for_step`.
///
/// Codex derives them from the step's executor capability discovery; an interrupted turn reuses
/// its last executing step's. A provider answers for `run`'s current step, or for an interrupted
/// run its last one. Without a provider, Stop and Interrupt see no executor hooks.
pub trait ExecutorHookSourceProvider: Send + Sync {
    /// The executor plugin hook sources of the step `run` executes or last executed.
    fn executor_hook_sources(
        &self,
        run: &ra_core::context::RunContext,
    ) -> Vec<crate::plugins::ExecutorPluginHookSource>;
}

/// One thread's product hooks, adapted to the runtime's host hook dispatcher.
pub struct ProductHookRuntime {
    hooks: RwLock<Arc<Hooks>>,
    /// The configuration the hooks were last built from, which a spawned agent's hooks start from.
    config: RwLock<HooksConfig>,
    session: ProductHookSession,
    /// Which thread the hooks speak for, which decides the lifecycle events they run.
    role: HookThreadRole,
    /// The hooks of the agents this thread spawned, refreshed with it.
    children: Mutex<Vec<std::sync::Weak<ProductHookRuntime>>>,
    unapplied: Mutex<Vec<UnappliedHookEffect>>,
    /// Where hook runs are reported: the thread's ordered event stream, once it is bound.
    events: Mutex<Option<mpsc::UnboundedSender<ThreadEvent>>>,
    background: BackgroundResults,
    context_delivery: Arc<Mutex<ContextDelivery>>,
    executor_hooks: OnceLock<Arc<dyn ExecutorHookSourceProvider>>,
    transcript: OnceLock<ra_runtime::thread::WeakAgentThread>,
    feedback_policy: OnceLock<FeedbackPolicy>,
    tool_hook_results: ToolHookResults,
}

/// Which thread a runtime's hooks speak for: Codex's session source, as far as it decides which
/// lifecycle hooks run.
pub(crate) enum HookThreadRole {
    /// A thread the host started: every lifecycle event runs.
    Root,
    /// A spawned agent's thread — live in its parent's tree, or resumed on its own: Codex's
    /// `SubAgent(ThreadSpawn)` session, which runs `SubagentStart` once instead of `SessionStart`,
    /// ends its turns with `SubagentStop`, and runs neither `Interrupt` nor `SessionEnd`.
    Child(ChildHookIdentity),
}

/// What a spawned agent's hooks know of it.
pub(crate) struct ChildHookIdentity {
    /// The agent type the spawn asked for, or Codex's default role name.
    agent_type: String,
    /// Codex's pending session start source: set for a spawned agent that has not started, taken
    /// by the first `SubagentStart` it receives.
    pending_start: Mutex<Option<ra_core::hook::SessionStartSource>>,
    /// The agent's own rollout, once its tree created it; a resumed thread uses its thread's.
    own_rollout: OnceLock<Arc<dyn RolloutRecorder>>,
    /// Where the transcript of the thread that spawned it is found.
    parent_transcript: ParentTranscript,
}

/// Where a spawned agent's hooks find their parent thread's transcript.
pub(crate) enum ParentTranscript {
    /// The hooks of the thread that spawned it, live in the same process.
    Runtime(std::sync::Weak<ProductHookRuntime>),
    /// A lookup in the store, as Codex reads the parent thread's rollout path when it needs it.
    Lookup(ParentTranscriptLookup),
}

/// Reads the parent thread's transcript path from the store when called.
pub(crate) type ParentTranscriptLookup =
    Arc<dyn Fn() -> futures::future::BoxFuture<'static, Option<PathBuf>> + Send + Sync>;

impl ChildHookIdentity {
    /// A spawned agent that has not started: its first run takes `start`.
    pub(crate) fn spawned(
        agent_type: Option<&str>,
        start: ra_core::hook::SessionStartSource,
        parent: &Arc<ProductHookRuntime>,
    ) -> Self {
        Self {
            agent_type: agent_type.unwrap_or(DEFAULT_AGENT_TYPE).to_owned(),
            pending_start: Mutex::new(Some(start)),
            own_rollout: OnceLock::new(),
            parent_transcript: ParentTranscript::Runtime(Arc::downgrade(parent)),
        }
    }

    /// A spawned agent's thread resumed on its own: nothing is pending, as Codex's resumed
    /// sub-agent session runs no start hook.
    pub(crate) fn resumed(agent_type: Option<&str>, parent: ParentTranscriptLookup) -> Self {
        Self {
            agent_type: agent_type.unwrap_or(DEFAULT_AGENT_TYPE).to_owned(),
            pending_start: Mutex::new(None),
            own_rollout: OnceLock::new(),
            parent_transcript: ParentTranscript::Lookup(parent),
        }
    }
}

/// Codex's `DEFAULT_ROLE_NAME`, the agent type of a spawn that named none.
pub(crate) const DEFAULT_AGENT_TYPE: &str = "default";

/// Tool results captured by invocations, keyed by run and call, for their own `PostToolUse` hooks.
pub(crate) type ToolHookResults = Arc<
    Mutex<
        std::collections::HashMap<(ra_core::state::RunId, ra_core::item::CallId), ToolHookRecord>,
    >,
>;

#[derive(Debug)]
pub(crate) enum ToolHookRecord {
    Mcp(McpHookResult),
    Exec(ra_exec::session::ExecCompletion),
}

pub(crate) struct CodingExecCompletionObserver;
impl ra_tools::exec_command::ExecCompletionObserver for CodingExecCompletionObserver {
    fn completed(
        &self,
        context: &ra_core::tool::ToolContext<'_>,
        completion: &ra_exec::session::ExecCompletion,
    ) {
        if let Some(results) = context
            .run()
            .app_context::<crate::thread_settings::CodingThreadSettings>()
            .and_then(|settings| settings.tool_hook_results.as_ref())
        {
            results
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(
                    (context.run().run_id().clone(), context.call_id().clone()),
                    ToolHookRecord::Exec(completion.clone()),
                );
        }
    }
}

#[derive(Debug)]
pub(crate) struct McpHookResult {
    pub(crate) result: ra_mcp::rmcp::model::CallToolResult,
    pub(crate) input: Value,
    pub(crate) token_limit: Option<usize>,
}

/// Model metadata belongs to the session; the selected model belongs to each active run.
#[derive(Clone)]
struct FeedbackPolicy {
    models: Vec<(String, ra_core::sandbox::token_truncation::TruncationPolicy)>,
    token_limit: Option<usize>,
}
impl FeedbackPolicy {
    fn for_model(&self, model: &str) -> ra_core::sandbox::token_truncation::TruncationPolicy {
        use ra_core::sandbox::token_truncation::{TruncationMode, TruncationPolicy};
        // Codex's unknown-model descriptor uses a byte budget.
        let policy = crate::config::catalog::find_model_by_slug(model, &self.models)
            .unwrap_or(TruncationPolicy::bytes(10_000));
        match self.token_limit {
            Some(limit) => match policy.mode() {
                TruncationMode::Bytes => TruncationPolicy::bytes(
                    i64::try_from(limit.saturating_mul(4)).unwrap_or(i64::MAX),
                ),
                _ => TruncationPolicy::tokens(i64::try_from(limit).unwrap_or(i64::MAX)),
            },
            None => policy,
        }
    }
}

/// The receiving end of finished background handlers and the task that delivers them.
struct BackgroundResults {
    receiver: async_channel::Receiver<HookCompletedEvent>,
    forwarder: Mutex<Option<tokio::task::JoinHandle<()>>>,
    stopped: AtomicBool,
}

impl ProductHookRuntime {
    /// Builds hooks from `config` for one thread, as the source does when a session starts.
    ///
    /// # Errors
    ///
    /// When a required managed hook fails to load.
    pub fn new(
        config: HooksConfig,
        session: ProductHookSession,
        mcp_executor: Arc<dyn super::HookMcpExecutor>,
    ) -> anyhow::Result<Arc<Self>> {
        Self::with_role(config, session, mcp_executor, HookThreadRole::Root)
    }

    /// The hooks of a thread whose role is `role`.
    pub(crate) fn with_role(
        config: HooksConfig,
        session: ProductHookSession,
        mcp_executor: Arc<dyn super::HookMcpExecutor>,
        role: HookThreadRole,
    ) -> anyhow::Result<Arc<Self>> {
        let (hooks, receiver) =
            Hooks::new(config.clone(), session.session_id.clone(), mcp_executor)?;
        let context_delivery = Arc::new(Mutex::new(ContextDelivery::default()));
        let delivery = Arc::clone(&context_delivery);
        let results = receiver.clone();
        // Codex drains finished background handlers at turn boundaries: their context and
        // warnings reach the current turn, and no client notification is sent for them, as its
        // notification filter drops background runs.
        let forwarder = tokio::spawn(async move {
            while let Ok(event) = results.recv().await {
                let mut contexts = Vec::new();
                let mut warnings = Vec::new();
                for entry in event.run.entries {
                    match entry.kind {
                        HookOutputEntryKind::Context => {
                            contexts.push(Message::developer(entry.text));
                        }
                        HookOutputEntryKind::Warning => warnings.push(entry.text),
                        _ => {}
                    }
                }
                delivery
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .deliver_background(contexts, warnings);
            }
        });
        Ok(Arc::new(Self {
            hooks: RwLock::new(Arc::new(hooks)),
            config: RwLock::new(config),
            session,
            role,
            children: Mutex::new(Vec::new()),
            unapplied: Mutex::new(Vec::new()),
            events: Mutex::new(None),
            background: BackgroundResults {
                receiver,
                forwarder: Mutex::new(Some(forwarder)),
                stopped: AtomicBool::new(false),
            },
            context_delivery,
            executor_hooks: OnceLock::new(),
            transcript: OnceLock::new(),
            feedback_policy: OnceLock::new(),
            tool_hook_results: ToolHookResults::default(),
        }))
    }

    /// The invocation-owned tool results and presentation limits for this thread.
    pub(crate) fn tool_hook_results(&self) -> ToolHookResults {
        Arc::clone(&self.tool_hook_results)
    }

    /// Drops results no post hook consumed in `turn`, including those of its nested runs.
    pub(crate) fn finish_turn_tool_results(&self, turn: &str) {
        let turn_of = self
            .context_delivery
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .turn_of
            .clone();
        self.tool_hook_results
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|(run, _), _| {
                let run = run.as_str();
                run != turn && turn_of.get(run).is_none_or(|owner| owner != turn)
            });
    }

    /// Captures the session model catalog; each post hook selects its active run's model budget.
    pub(crate) fn set_feedback_policy(&self, config: &crate::config::Config) {
        use ra_core::sandbox::token_truncation::TruncationPolicy;
        let models = config
            .model_catalog
            .iter()
            .flat_map(|catalog| catalog.models.iter())
            .map(|model| {
                let policy = match model.truncation_policy.mode {
                    crate::config::catalog::TruncationMode::Bytes => {
                        TruncationPolicy::bytes(model.truncation_policy.limit)
                    }
                    crate::config::catalog::TruncationMode::Tokens => {
                        TruncationPolicy::tokens(model.truncation_policy.limit)
                    }
                };
                (model.slug.clone(), policy)
            })
            .collect();
        let _ = self.feedback_policy.set(FeedbackPolicy {
            models,
            token_limit: config.tool_output_token_limit,
        });
    }

    /// Applies a refreshed configuration while background handlers keep running, to these hooks
    /// and to those of every agent this thread spawned that is still alive, as Codex rebuilds the
    /// hook runtime of every loaded thread.
    pub fn refresh(&self, config: HooksConfig) {
        let next = Arc::new(self.current().reconfigured(config.clone()));
        *self.hooks.write().unwrap_or_else(PoisonError::into_inner) = next;
        let children: Vec<_> = {
            let mut children = self.children.lock().unwrap_or_else(PoisonError::into_inner);
            children.retain(|child| child.strong_count() > 0);
            children
                .iter()
                .filter_map(std::sync::Weak::upgrade)
                .collect()
        };
        for child in children {
            child.refresh(config.clone());
        }
        *self.config.write().unwrap_or_else(PoisonError::into_inner) = config;
    }

    /// The hooks of an agent this thread spawns: built from the configuration these hooks run
    /// with now, as Codex's spawned session builds its hooks from the configuration it copies, for
    /// the session `session`, with their own background results and delivery, and refreshed with
    /// these. Notices about its hook runs reach this thread's connections, naming the agent's
    /// thread.
    pub(crate) fn spawn_child(
        self: &Arc<Self>,
        session: ProductHookSession,
        identity: ChildHookIdentity,
        mcp_executor: Arc<dyn super::HookMcpExecutor>,
    ) -> anyhow::Result<Arc<Self>> {
        let config = self
            .config
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let child = Self::with_role(
            config,
            session,
            mcp_executor,
            HookThreadRole::Child(identity),
        )?;
        if let Some(policy) = self.feedback_policy.get() {
            let _ = child.feedback_policy.set(policy.clone());
        }
        if let Some(events) = self
            .events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
        {
            child.bind_thread_events(events);
        }
        self.children
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Arc::downgrade(&child));
        Ok(child)
    }

    /// The spawned agent's own rollout, created by its tree: what its hooks report as their
    /// transcript.
    pub(crate) fn set_own_rollout(&self, recorder: &Arc<dyn RolloutRecorder>) {
        if let HookThreadRole::Child(child) = &self.role {
            let _ = child.own_rollout.set(Arc::clone(recorder));
        }
    }

    /// The session the hooks were built for.
    pub(crate) const fn session(&self) -> &ProductHookSession {
        &self.session
    }

    /// Whether these hooks speak for a spawned agent's thread.
    pub(crate) const fn is_child(&self) -> bool {
        matches!(self.role, HookThreadRole::Child(_))
    }

    /// Codex's `thread_spawn_subagent_hook_context`: present only for a spawned agent's thread.
    fn subagent(&self) -> Option<super::events::common::SubagentHookContext> {
        match &self.role {
            HookThreadRole::Root => None,
            HookThreadRole::Child(child) => Some(super::events::common::SubagentHookContext {
                agent_id: self.session.session_id.to_string(),
                agent_type: child.agent_type.clone(),
            }),
        }
    }

    /// The transcript of the thread that spawned this one, for its `SubagentStop`.
    async fn parent_transcript_path(&self) -> Option<PathBuf> {
        let HookThreadRole::Child(child) = &self.role else {
            return None;
        };
        match &child.parent_transcript {
            ParentTranscript::Runtime(parent) => {
                let parent = parent.upgrade()?;
                Box::pin(parent.transcript_path(false)).await
            }
            ParentTranscript::Lookup(lookup) => lookup().await,
        }
    }

    /// The hooks used by the next dispatch.
    #[must_use]
    pub fn current(&self) -> Arc<Hooks> {
        self.hooks
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Reports this thread's hook runs on `events`, the thread's ordered event stream: started
    /// and completed runs as `hook/started` and `hook/completed` notices. Replaces an earlier
    /// sender.
    pub fn bind_thread_events(&self, events: mpsc::UnboundedSender<ThreadEvent>) {
        *self.events.lock().unwrap_or_else(PoisonError::into_inner) = Some(events);
    }

    /// Queries the loaded thread for each request without retaining the thread.
    pub fn bind_transcript(&self, thread: ra_runtime::thread::WeakAgentThread) {
        let _ = self.transcript.set(thread);
    }

    async fn transcript_path(&self, flush: bool) -> Option<PathBuf> {
        // A spawned agent's own rollout, materialized first, as Codex's `hook_transcript_path` does.
        if let HookThreadRole::Child(child) = &self.role
            && let Some(rollout) = child.own_rollout.get()
        {
            let path = rollout.local_path()?;
            if let Err(error) = rollout.persist(PersistContext::Standard).await {
                tracing::warn!(%error, "the hook transcript could not be materialized");
            }
            if flush && let Err(error) = rollout.flush().await {
                tracing::warn!(%error, "failed to flush transcript before lifecycle hook");
            }
            return Some(path);
        }
        let Some(source) = self.transcript.get() else {
            return self.session.transcript_path.clone();
        };
        let thread = source.upgrade()?;
        let path = thread.hook_transcript_path().await;
        if flush && let Err(error) = thread.flush_rollout().await {
            tracing::warn!(%error, "failed to flush transcript before lifecycle hook");
        }
        path
    }

    /// Stops reporting hook runs.
    pub fn release_thread_events(&self) {
        self.events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
    }

    /// Supplies the executor plugin hook sources Stop and Interrupt add, as Codex adds those of
    /// the step they concern. Only the first provider is kept.
    pub fn set_executor_hook_sources(&self, provider: Arc<dyn ExecutorHookSourceProvider>) {
        let _ = self.executor_hooks.set(provider);
    }

    /// Warnings collected while the hooks were loaded, which Codex shows when the session starts.
    #[must_use]
    pub fn startup_warnings(&self) -> Vec<String> {
        self.current().startup_warnings().to_vec()
    }

    /// Stops background work as Codex's session shutdown does before its `SessionEnd` hooks: the
    /// background concurrency gate closes and running background handlers are aborted and
    /// awaited, the result channel closes, and results nobody took — including those waiting for
    /// the next turn — are dropped. Idempotent.
    pub async fn stop_background(&self) {
        if self.background.stopped.swap(true, Ordering::SeqCst) {
            return;
        }
        self.current().shutdown().await;
        self.background.receiver.close();
        let forwarder = self
            .background
            .forwarder
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(forwarder) = forwarder {
            forwarder.abort();
            let _ = forwarder.await;
        }
        while self.background.receiver.try_recv().is_ok() {}
        self.context_delivery
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .discard_pending();
    }

    /// Legacy diagnostic drain retained for source compatibility. Implemented effects are applied
    /// directly and no longer appear here.
    pub fn take_unapplied_effects(&self) -> Vec<UnappliedHookEffect> {
        std::mem::take(
            &mut *self
                .unapplied
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        )
    }

    /// Registrations for every event the product hooks handle.
    #[must_use]
    pub fn user_hooks(self: &Arc<Self>) -> UserHooks {
        [
            HookEventName::SessionStart,
            HookEventName::SessionEnd,
            HookEventName::UserPromptSubmit,
            HookEventName::PreToolUse,
            HookEventName::PermissionRequest,
            HookEventName::PostToolUse,
            HookEventName::Stop,
            HookEventName::SubagentStart,
            HookEventName::SubagentStop,
            HookEventName::Interrupt,
            HookEventName::PreCompact,
            HookEventName::PostCompact,
        ]
        .into_iter()
        .fold(
            UserHooks::default().with_session_id(self.session.session_id.clone()),
            |hooks, event| {
                hooks.with_hook(UserHookRegistration::new(
                    event,
                    Arc::new(ProductHook {
                        runtime: Arc::clone(self),
                    }),
                ))
            },
        )
    }

    /// Codex's `run_permission_request_hooks` for an approval that is not a tool call, which the
    /// runtime's hook dispatcher is never asked about: the access is described to the hooks as
    /// the shell command asking, with `description`, under the run `run_id_suffix` of `turn_id`.
    pub async fn run_permission_request_hooks(
        &self,
        turn_id: &str,
        run_id_suffix: &str,
        command: &str,
        description: &str,
    ) -> Option<PermissionRequestDecision> {
        self.run_permission_request_hooks_with_settings(
            turn_id,
            run_id_suffix,
            command,
            description,
            Some(&self.session.model),
            self.session.approval_policy,
        )
        .await
    }

    /// Runs a non-tool approval with the model and policy captured by its review context.
    #[allow(clippy::too_many_arguments)]
    pub async fn run_permission_request_hooks_with_settings(
        &self,
        turn_id: &str,
        run_id_suffix: &str,
        command: &str,
        description: &str,
        model: Option<&str>,
        approval_policy: AskForApproval,
    ) -> Option<PermissionRequestDecision> {
        let session = &self.session;
        let request = PermissionRequestRequest {
            session_id: session.session_id.clone(),
            turn_id: turn_id.to_owned(),
            subagent: self.subagent(),
            cwd: session.cwd.to_path_buf(),
            transcript_path: self.transcript_path(false).await,
            model: model.map_or_else(|| {
                tracing::debug!(%turn_id, "network review has no resolved model; using session fallback");
                session.model.clone()
            }, str::to_owned),
            permission_mode: hook_permission_mode(approval_policy),
            tool_name: "Bash".to_owned(),
            matcher_aliases: Vec::new(),
            run_id_suffix: run_id_suffix.to_owned(),
            tool_input: serde_json::json!({"command": command, "description": description}),
        };
        let hooks = self.current();
        self.emit_started(turn_id, hooks.preview_permission_request(&request));
        let outcome = hooks.run_permission_request(request).await;
        self.emit_completed(outcome.hook_events);
        outcome.decision
    }

    /// Codex's `emit_hook_started_events`: the previewed runs, minus built-in and background ones.
    fn emit_started(&self, turn_id: &str, runs: Vec<HookRunSummary>) {
        for run in runs.into_iter().filter(should_emit_hook_notification) {
            self.notice(
                Some(turn_id.to_owned()),
                HOOK_STARTED_METHOD,
                &HookStartedEvent {
                    turn_id: Some(turn_id.to_owned()),
                    run,
                },
            );
        }
    }

    /// Codex's `emit_hook_completed_events`, for its client notifications: completed runs, minus
    /// built-in and background ones.
    fn emit_completed(&self, events: Vec<HookCompletedEvent>) {
        for event in events
            .into_iter()
            .filter(|event| should_emit_hook_notification(&event.run))
        {
            self.notice(event.turn_id.clone(), HOOK_COMPLETED_METHOD, &event);
        }
    }

    fn notice(&self, turn_id: Option<String>, method: &str, params: &impl serde::Serialize) {
        let Some(events) = self
            .events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
        else {
            return;
        };
        match serde_json::to_value(params) {
            Ok(params) => {
                let mut notice = ThreadNotice::new(turn_id, method, params);
                if self.is_child() {
                    notice = notice.with_thread_id(self.session.session_id.clone());
                }
                let _ = events.send(ThreadEvent::Notice(notice));
            }
            Err(error) => tracing::warn!(%error, %method, "hook notice could not be serialized"),
        }
    }

    fn record_contexts(&self, context: &UserHookContext<'_>, contexts: Vec<String>) {
        let messages: Vec<Message> = contexts.into_iter().map(Message::developer).collect();
        if messages.is_empty() {
            return;
        }
        let rejected = match context.context_output() {
            Some(queue) => queue.push(messages).err().unwrap_or_default(),
            None => messages,
        };
        self.context_delivery
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pending
            .extend(rejected);
    }

    /// Whether `run` is a turn of these hooks' thread rather than a run nested in one.
    fn is_turn_root(&self, run: &ra_core::context::RunContext) -> bool {
        self.turn_id(run) == run.run_id().as_str()
    }

    /// The thread turn `run` belongs to: Codex's turn `sub_id`, the root run's identity.
    pub(crate) fn turn_id(&self, run: &ra_core::context::RunContext) -> String {
        let run_id = run.run_id().as_str();
        if let Some(turn) = self
            .context_delivery
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .turn_of
            .get(run_id)
        {
            return turn.clone();
        }
        tracing::debug!(%run_id, "hook run outside a bound turn; using the run's identity");
        run_id.to_owned()
    }

    fn executor_hook_sources(
        &self,
        run: &ra_core::context::RunContext,
    ) -> Vec<crate::plugins::ExecutorPluginHookSource> {
        self.executor_hooks
            .get()
            .map(|provider| provider.executor_hook_sources(run))
            .unwrap_or_default()
    }

    fn permission_mode(&self) -> String {
        hook_permission_mode(self.session.approval_policy)
    }

    /// Codex's `run_legacy_after_agent_hook`: the legacy `notify` program, told the turn completed.
    /// Returns the error to end the turn with when a hook aborted it.
    async fn run_legacy_after_agent(
        &self,
        hooks: &Hooks,
        turn_id: &str,
        input: &[ModelInputItem],
        last_assistant_message: Option<String>,
    ) -> Option<String> {
        let input_messages = sampled_user_messages(input);
        let mut abort_message = None;
        for outcome in hooks
            .dispatch(HookPayload {
                session_id: self.session.session_id.clone(),
                cwd: self.session.cwd.clone(),
                client: None,
                triggered_at: chrono::Utc::now(),
                hook_event: super::HookEvent::AfterAgent {
                    event: HookEventAfterAgent {
                        thread_id: self.session.session_id.clone(),
                        turn_id: turn_id.to_owned(),
                        input_messages,
                        last_assistant_message,
                    },
                },
            })
            .await
        {
            let hook_name = outcome.hook_name;
            let (error, should_abort) = match outcome.result {
                HookResult::Success => continue,
                HookResult::FailedContinue(error) => (error, false),
                HookResult::FailedAbort(error) => (error, true),
            };
            let action = if should_abort {
                "aborting operation"
            } else {
                "continuing"
            };
            tracing::warn!(%turn_id, %hook_name, %error, "after_agent hook failed; {action}");
            if should_abort && abort_message.is_none() {
                abort_message = Some(format!(
                    "after_agent hook '{hook_name}' failed and aborted turn completion: {error}"
                ));
            }
        }
        abort_message
    }

    #[allow(
        clippy::too_many_lines,
        reason = "One arm per event, as the source hook runtime has one function per event."
    )]
    async fn dispatch(&self, context: &UserHookContext<'_>, event: &HookEvent<'_>) -> HookDecision {
        let hooks = self.current();
        let session = &self.session;
        let turn_id = self.turn_id(context.run());
        let model = context.run().resolved_model_name().map_or_else(
            || {
                tracing::debug!(%turn_id, "hook context has no resolved model; using session fallback");
                session.model.clone()
            },
            str::to_owned,
        );
        let permission_mode = context.run().app_context::<crate::thread_settings::CodingThreadSettings>()
            .map_or_else(|| {
                tracing::debug!(%turn_id, "hook context has no product settings; using session fallback");
                self.permission_mode()
            }, |settings| hook_permission_mode(settings.approval_policy()));
        match *event {
            // Codex's sub-agent sessions run no start hook other than `SubagentStart`, whatever
            // the source — a resumed or compacted spawned thread included.
            HookEvent::SessionStart { .. } if self.is_child() => HookDecision::Continue,
            HookEvent::SessionStart { source, .. } => {
                let request = SessionStartRequest {
                    session_id: session.session_id.clone(),
                    cwd: session.cwd.clone(),
                    transcript_path: self.transcript_path(false).await,
                    model: model.clone(),
                    permission_mode: permission_mode.clone(),
                    target: StartHookTarget::SessionStart { source },
                };
                self.emit_started(&turn_id, hooks.preview_session_start(&request));
                let outcome = hooks
                    .run_session_start(request, Some(turn_id.clone()))
                    .await;
                self.emit_completed(outcome.hook_events);
                self.record_contexts(context, outcome.additional_contexts);
                if outcome.should_stop {
                    return HookDecision::Deny {
                        message: non_empty(outcome.stop_reason, "Hook stopped prompt processing"),
                    };
                }
                HookDecision::Continue
            }
            HookEvent::SessionEnd { .. } => {
                // Background work stops first, whether or not SessionEnd has handlers, so no
                // background handler runs or reports after this point.
                self.stop_background().await;
                let preview = hooks.preview_session_end();
                // `SessionEnd` is root-only, as in Codex.
                if preview.is_empty() || self.is_child() {
                    return HookDecision::Continue;
                }
                let request = SessionEndRequest {
                    session_id: session.session_id.clone(),
                    turn_id: turn_id.clone(),
                    cwd: session.cwd.clone(),
                    transcript_path: self.transcript_path(true).await,
                };
                self.emit_started(&turn_id, preview);
                let outcome = hooks.run_session_end(request).await;
                self.emit_completed(outcome.hook_events);
                HookDecision::Continue
            }
            HookEvent::UserPromptSubmit { input } => {
                let prompt = user_prompt(input);
                let request = UserPromptSubmitRequest {
                    session_id: session.session_id.clone(),
                    turn_id: turn_id.clone(),
                    subagent: self.subagent(),
                    cwd: session.cwd.clone(),
                    transcript_path: self.transcript_path(false).await,
                    model: model.clone(),
                    permission_mode: permission_mode.clone(),
                    prompt,
                };
                self.emit_started(&turn_id, hooks.preview_user_prompt_submit(&request));
                let outcome = hooks.run_user_prompt_submit(request).await;
                self.emit_completed(outcome.hook_events);
                self.record_contexts(context, outcome.additional_contexts);
                if outcome.should_stop {
                    return HookDecision::Deny {
                        message: non_empty(outcome.stop_reason, "Hook stopped prompt processing"),
                    };
                }
                HookDecision::Continue
            }
            HookEvent::PreToolUse(call) => {
                // A patch that cannot be parsed fails before any hook, as Codex resolves a patch's
                // environment, and parses it, before its pre-tool hooks: the call then fails at
                // decoding with `apply_patch verification failed`. A command whose argument text
                // is not JSON has no hook input either, as Codex's exec handler builds its payload
                // only from arguments it could parse.
                if exec_kind(&call) == Some(ExecKind::Stdin)
                    || is_unparsable_patch(&call)
                    || (exec_kind(&call) == Some(ExecKind::Command)
                        && call.has_unparsed_arguments())
                {
                    return HookDecision::Continue;
                }
                let tool = HookToolName::for_call(&call);
                let tool_use_id = call.call_id().to_string();
                let request = PreToolUseRequest {
                    session_id: session.session_id.clone(),
                    turn_id: turn_id.clone(),
                    subagent: self.subagent(),
                    cwd: session.cwd.clone(),
                    transcript_path: self.transcript_path(false).await,
                    model: model.clone(),
                    permission_mode: permission_mode.clone(),
                    tool_name: tool.name.clone(),
                    matcher_aliases: tool.matcher_aliases.clone(),
                    tool_use_id: tool_use_id.clone(),
                    tool_input: hook_input(&call),
                };
                self.emit_started(&turn_id, hooks.preview_pre_tool_use(&request));
                let outcome = hooks.run_pre_tool_use(request).await;
                self.emit_completed(outcome.hook_events);
                self.record_contexts(context, outcome.additional_contexts);
                if !outcome.should_block {
                    return outcome
                        .updated_input
                        .map_or(HookDecision::Continue, |input| {
                            match native_input(&call, input) {
                                Ok(input) => HookDecision::UpdateInput { input },
                                Err(message) => HookDecision::Deny { message },
                            }
                        });
                }
                let Some(reason) = outcome.block_reason else {
                    return HookDecision::Continue;
                };
                HookDecision::Deny {
                    message: blocked_tool_message(&tool, &hook_input(&call), &reason),
                }
            }
            HookEvent::PermissionRequest(call) => {
                let tool = HookToolName::for_call(&call);
                let request = PermissionRequestRequest {
                    session_id: session.session_id.clone(),
                    turn_id: turn_id.clone(),
                    subagent: self.subagent(),
                    cwd: session.cwd.to_path_buf(),
                    transcript_path: self.transcript_path(false).await,
                    model: model.clone(),
                    permission_mode: permission_mode.clone(),
                    tool_name: tool.name,
                    matcher_aliases: tool.matcher_aliases,
                    run_id_suffix: permission_run_id_suffix(&call),
                    tool_input: permission_input(&call),
                };
                self.emit_started(&turn_id, hooks.preview_permission_request(&request));
                let outcome = hooks.run_permission_request(request).await;
                self.emit_completed(outcome.hook_events);
                match outcome.decision {
                    Some(PermissionRequestDecision::Allow) => HookDecision::Allow,
                    Some(PermissionRequestDecision::Deny { message }) => {
                        HookDecision::Deny { message }
                    }
                    None => HookDecision::Continue,
                }
            }
            HookEvent::PostToolUse { call, output } => {
                // The invocation retains its original result and limit from its immutable binding.
                // Consume it by call identity, never by a mutable discovery name cache.
                let record = self
                    .tool_hook_results
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&(context.run().run_id().clone(), call.call_id().clone()));
                let (mcp_result, exec_result) = match record {
                    Some(ToolHookRecord::Mcp(result)) => (Some(result), None),
                    Some(ToolHookRecord::Exec(result)) => (None, Some(result)),
                    None => (None, None),
                };
                if call.tool().is_some() && exec_kind(&call).is_some() && exec_result.is_none() {
                    return HookDecision::Continue;
                }
                if mcp_result
                    .as_ref()
                    .is_some_and(|result| result.result.is_error == Some(true))
                {
                    return HookDecision::Continue;
                }
                let token_limit = mcp_result.as_ref().and_then(|result| result.token_limit);
                let feedback_limit = self.feedback_policy.get().map(|policy| {
                    token_limit.map_or_else(
                        || policy.for_model(&model),
                        |limit| {
                            ra_core::sandbox::token_truncation::TruncationPolicy::tokens(
                                i64::try_from(limit).unwrap_or(i64::MAX),
                            )
                        },
                    )
                });
                let truncate_feedback = |message: String| match feedback_limit {
                    Some(policy) => {
                        use ra_core::sandbox::token_truncation::{
                            TruncationMode, TruncationPolicy, truncate_text,
                        };
                        let allowance = policy.limit().saturating_mul(6) / 5;
                        let allowance = i64::try_from(allowance).unwrap_or(i64::MAX);
                        let policy = match policy.mode() {
                            TruncationMode::Bytes => TruncationPolicy::bytes(allowance),
                            _ => TruncationPolicy::tokens(allowance),
                        };
                        truncate_text(&message, policy)
                    }
                    None => message,
                };
                let tool = HookToolName::for_call(&call);
                let tool_response = mcp_result.as_ref().map_or_else(
                    || {
                        output.as_text().map_or_else(
                            || serde_json::to_value(output).unwrap_or(Value::Null),
                            |text| Value::String(text.to_owned()),
                        )
                    },
                    |result| serde_json::to_value(&result.result).unwrap_or(Value::Null),
                );
                let (tool, tool_use_id, tool_input, tool_response) =
                    if let Some(completion) = &exec_result {
                        use ra_core::sandbox::token_truncation::TruncationPolicy;
                        let requested = TruncationPolicy::tokens(10_000);
                        let model_policy = self
                            .feedback_policy
                            .get()
                            .map_or(TruncationPolicy::bytes(10_000), |policy| {
                                policy.for_model(&model)
                            });
                        let policy = if requested.byte_budget() < model_policy.byte_budget() {
                            requested
                        } else {
                            model_policy
                        };
                        (
                            HookToolName {
                                name: "Bash".to_owned(),
                                matcher_aliases: Vec::new(),
                            },
                            completion
                                .origin_call_id()
                                .filter(|id| !id.is_empty())
                                .unwrap_or(call.call_id().as_str())
                                .to_owned(),
                            serde_json::json!({"command": completion.command()}),
                            Value::String(ra_tools::exec_command::truncated_exec_output(
                                completion.output().unwrap_or_default(),
                                completion.omitted_bytes(),
                                completion.total_bytes(),
                                policy,
                            )),
                        )
                    } else {
                        (
                            tool,
                            call.call_id().to_string(),
                            mcp_result
                                .as_ref()
                                .map_or_else(|| hook_input(&call), |result| result.input.clone()),
                            tool_response,
                        )
                    };
                let request = PostToolUseRequest {
                    session_id: session.session_id.clone(),
                    turn_id: turn_id.clone(),
                    subagent: self.subagent(),
                    cwd: session.cwd.clone(),
                    transcript_path: self.transcript_path(false).await,
                    model: model.clone(),
                    permission_mode: permission_mode.clone(),
                    tool_name: tool.name,
                    matcher_aliases: tool.matcher_aliases,
                    tool_use_id,
                    tool_input,
                    tool_response,
                };
                self.emit_started(&turn_id, hooks.preview_post_tool_use(&request));
                let outcome = hooks.run_post_tool_use(request).await;
                self.emit_completed(outcome.hook_events);
                self.record_contexts(context, outcome.additional_contexts);
                if outcome.should_block {
                    return HookDecision::Deny {
                        message: truncate_feedback(outcome.feedback_message.unwrap_or_else(|| {
                            "PostToolUse hook blocked the tool result".to_owned()
                        })),
                    };
                }
                outcome
                    .feedback_message
                    .map_or(HookDecision::Continue, |message| {
                        HookDecision::ReplaceOutput {
                            message: truncate_feedback(message),
                        }
                    })
            }
            HookEvent::SubagentStart { .. } => {
                // Codex's pending start source: a spawned agent's first run takes it, once; later
                // runs, approval resumes, nested runs and a resumed thread find nothing to take.
                let HookThreadRole::Child(child) = &self.role else {
                    return HookDecision::Continue;
                };
                if !self.is_turn_root(context.run()) {
                    return HookDecision::Continue;
                }
                let pending = child
                    .pending_start
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .take();
                if pending.is_none() {
                    return HookDecision::Continue;
                }
                let request = SessionStartRequest {
                    session_id: session.session_id.clone(),
                    cwd: session.cwd.clone(),
                    transcript_path: self.transcript_path(false).await,
                    model: model.clone(),
                    permission_mode: permission_mode.clone(),
                    target: StartHookTarget::SubagentStart {
                        turn_id: turn_id.clone(),
                        agent_id: session.session_id.to_string(),
                        agent_type: child.agent_type.clone(),
                    },
                };
                self.emit_started(&turn_id, hooks.preview_session_start(&request));
                let outcome = hooks
                    .run_session_start(request, Some(turn_id.clone()))
                    .await;
                self.emit_completed(outcome.hook_events);
                self.record_contexts(context, outcome.additional_contexts);
                // `continue: false` does not stop a spawned agent, as in Codex.
                HookDecision::Continue
            }
            HookEvent::Stop(delivery) | HookEvent::SubagentStop { delivery, .. } => {
                // A spawned agent's turn ends with `SubagentStop`, reporting its parent thread's
                // transcript and its own, as Codex's `ThreadSpawn` turns do — a resumed spawned
                // thread's top-level turn included. A nested agent-tool run records into its
                // thread's rollout and has no transcript of its own, so only that thread's path is
                // known for it.
                let spawned_turn = self.is_child() && self.is_turn_root(context.run());
                let event_name = if spawned_turn {
                    HookEventName::SubagentStop
                } else {
                    event.name()
                };
                let (transcript_path, target) = if spawned_turn {
                    let HookThreadRole::Child(child) = &self.role else {
                        unreachable!("checked above");
                    };
                    (
                        self.parent_transcript_path().await,
                        StopHookTarget::SubagentStop {
                            agent_id: session.session_id.to_string(),
                            agent_type: child.agent_type.clone(),
                            agent_transcript_path: self.transcript_path(false).await,
                        },
                    )
                } else if event_name == HookEventName::SubagentStop {
                    (
                        self.transcript_path(false).await,
                        StopHookTarget::SubagentStop {
                            agent_id: context.run().run_id().to_string(),
                            agent_type: context.run().agent().name().to_owned(),
                            agent_transcript_path: None,
                        },
                    )
                } else {
                    (self.transcript_path(false).await, StopHookTarget::Stop)
                };
                let last_assistant_message =
                    delivery.message().map(ra_core::item::Message::text_content);
                let request = StopRequest {
                    session_id: session.session_id.clone(),
                    turn_id: turn_id.clone(),
                    cwd: session.cwd.clone(),
                    transcript_path,
                    model: model.clone(),
                    permission_mode: permission_mode.clone(),
                    request_metadata: None,
                    stop_hook_active: delivery.stop_hook_active(),
                    last_assistant_message: last_assistant_message.clone(),
                    target,
                };
                let stop_hooks =
                    hooks.with_executor_hooks(self.executor_hook_sources(context.run()));
                self.emit_started(&turn_id, stop_hooks.preview_stop(&request));
                let mut outcome = stop_hooks.run_stop(request).await;
                self.emit_completed(std::mem::take(&mut outcome.hook_events));
                if outcome.should_stop {
                    return HookDecision::Deny {
                        message: non_empty(outcome.stop_reason, "Stop hook stopped processing"),
                    };
                }
                if outcome.should_block {
                    let prompt = outcome
                        .continuation_fragments
                        .iter()
                        .filter(|fragment| !fragment.hook_run_id.trim().is_empty())
                        .map(|fragment| {
                            format!(
                                "<hook_prompt hook_run_id=\"{}\">{}</hook_prompt>",
                                escape_xml(&fragment.hook_run_id),
                                escape_xml(&fragment.text)
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n\n");
                    if !prompt.trim().is_empty() {
                        return HookDecision::Block { prompt };
                    }
                    // Codex warns about a block without a prompt and goes on to end the turn.
                    self.warn(
                        "Stop hook requested continuation without a prompt; ignoring the block.",
                    );
                }
                if event_name == HookEventName::Stop
                    && let Some(message) = self
                        .run_legacy_after_agent(
                            &stop_hooks,
                            &turn_id,
                            delivery.sampled_input().unwrap_or_default(),
                            last_assistant_message,
                        )
                        .await
                {
                    // Codex reports the abort as the turn's error and ends the turn.
                    self.send_event(ThreadEvent::Error {
                        turn_id: turn_id.clone(),
                        message: message.clone(),
                    });
                    return HookDecision::Deny { message };
                }
                HookDecision::Continue
            }
            HookEvent::Interrupt { .. } => {
                // Codex runs Interrupt hooks once, for the turn: a nested agent-tool run's own
                // interruption is part of that turn's. A spawned agent's thread runs none, as
                // Codex skips them for every sub-agent session.
                if context.run().parent_run_id().is_some() || self.is_child() {
                    return HookDecision::Continue;
                }
                let executor_hook_sources = self.executor_hook_sources(context.run());
                let has_executor_hooks = !executor_hook_sources.is_empty();
                let hooks = hooks.with_executor_hooks(executor_hook_sources);
                let preview = hooks.preview_interrupt();
                if preview.is_empty() && !has_executor_hooks {
                    return HookDecision::Continue;
                }
                let request = InterruptRequest {
                    session_id: session.session_id.clone(),
                    turn_id: turn_id.clone(),
                    cwd: session.cwd.clone(),
                    transcript_path: self.transcript_path(true).await,
                    model: model.clone(),
                    permission_mode: permission_mode.clone(),
                    request_metadata: None,
                };
                self.emit_started(&turn_id, preview);
                let outcome = hooks.run_interrupt(request).await;
                self.emit_completed(outcome.hook_events);
                HookDecision::Continue
            }
            HookEvent::PreCompact { trigger, .. } => {
                let request = PreCompactRequest {
                    session_id: session.session_id.clone(),
                    turn_id: turn_id.clone(),
                    subagent: self.subagent(),
                    cwd: session.cwd.clone(),
                    transcript_path: self.transcript_path(false).await,
                    model: model.clone(),
                    trigger: compaction_trigger_label(trigger).to_owned(),
                };
                self.emit_started(&turn_id, hooks.preview_pre_compact(&request));
                let outcome = hooks.run_pre_compact(request).await;
                self.emit_completed(outcome.hook_events);
                if outcome.should_stop {
                    return HookDecision::Deny {
                        message: non_empty(
                            outcome.stop_reason,
                            "PreCompact hook stopped compaction",
                        ),
                    };
                }
                HookDecision::Continue
            }
            HookEvent::PostCompact { trigger, .. } => {
                let request = PostCompactRequest {
                    session_id: session.session_id.clone(),
                    turn_id: turn_id.clone(),
                    subagent: self.subagent(),
                    cwd: session.cwd.clone(),
                    transcript_path: self.transcript_path(false).await,
                    model: model.clone(),
                    trigger: compaction_trigger_label(trigger).to_owned(),
                };
                self.emit_started(&turn_id, hooks.preview_post_compact(&request));
                let outcome = hooks.run_post_compact(request).await;
                self.emit_completed(outcome.hook_events);
                if outcome.should_stop {
                    return HookDecision::Deny {
                        message: non_empty(
                            outcome.stop_reason,
                            "PostCompact hook stopped the turn",
                        ),
                    };
                }
                HookDecision::Continue
            }
            _ => HookDecision::Continue,
        }
    }

    fn warn(&self, message: &str) {
        self.send_event(ThreadEvent::Warning {
            message: message.to_owned(),
        });
    }

    fn send_event(&self, event: ThreadEvent) {
        if let Some(events) = self
            .events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
        {
            let _ = events.send(event);
        }
    }
}

impl Drop for ProductHookRuntime {
    fn drop(&mut self) {
        if let Some(forwarder) = self
            .background
            .forwarder
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            forwarder.abort();
        }
    }
}

struct ProductHook {
    runtime: Arc<ProductHookRuntime>,
}

#[async_trait]
impl UserHook for ProductHook {
    fn name(&self) -> &'static str {
        "product hooks"
    }

    fn bind_context(&self, run: &ra_core::context::RunContext, output: &HookContextQueue) {
        let mut delivery = self
            .runtime
            .context_delivery
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let run_id = run.run_id().to_string();
        // A spawned agent's hooks see their agent's runs, whose parents ran in the thread that
        // spawned it: each is a turn of the agent's own thread.
        let parent = run.parent_run_id().filter(|parent| {
            !self.runtime.is_child() || delivery.turn_of.contains_key(parent.as_str())
        });
        match parent {
            // A root run is the thread's turn; runs of other turns are forgotten.
            None => {
                delivery.turn_of.retain(|_, turn| *turn == run_id);
                delivery.turn_of.insert(run_id.clone(), run_id);
                delivery.root = Some(output.clone());
                delivery.deliver_pending();
            }
            // A nested run belongs to the turn of the run that started it, which binds first.
            Some(parent) => {
                let parent = parent.to_string();
                let turn = delivery.turn_of.get(&parent).cloned().unwrap_or(parent);
                delivery.turn_of.insert(run_id, turn);
            }
        }
    }

    async fn call(
        &self,
        context: &UserHookContext<'_>,
        event: &HookEvent<'_>,
    ) -> Result<HookDecision> {
        Ok(self.runtime.dispatch(context, event).await)
    }
}

/// The user messages of the input a turn was sampled with, as Codex's after-agent hook reads them
/// through `parse_turn_item`: hook prompts and model-only contextual messages are not the user's.
fn sampled_user_messages(input: &[ModelInputItem]) -> Vec<String> {
    input
        .iter()
        .filter_map(|item| match item {
            ModelInputItem::Message(message)
                if message.role() == MessageRole::User
                    && !ra_core::session::contextual::is_contextual_user_message(message) =>
            {
                let text = message.text_content();
                (!text.trim_start().starts_with("<hook_prompt ")).then_some(text)
            }
            _ => None,
        })
        .collect()
}

/// Codex's `should_emit_hook_notification`: built-in and background runs keep their telemetry but
/// are not shown to clients.
fn should_emit_hook_notification(run: &HookRunSummary) -> bool {
    !run.builtin && run.execution_mode == HookExecutionMode::Sync
}

/// Codex's `compaction_trigger_label`.
const fn compaction_trigger_label(trigger: CompactTrigger) -> &'static str {
    match trigger {
        CompactTrigger::Manual => "manual",
        _ => "auto",
    }
}

fn non_empty(reason: Option<String>, fallback: &str) -> String {
    reason
        .filter(|reason| !reason.trim().is_empty())
        .unwrap_or_else(|| fallback.to_owned())
}

/// Selected shell transport, identified by instance whenever available.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExecKind {
    Command,
    Stdin,
}

fn exec_kind(call: &ToolHookData<'_>) -> Option<ExecKind> {
    if let Some(tool) = call.tool() {
        let tool = tool as &dyn std::any::Any;
        if tool
            .downcast_ref::<ra_tools::exec_command::ExecCommandTool>()
            .is_some()
        {
            return Some(ExecKind::Command);
        }
        if tool
            .downcast_ref::<ra_tools::write_stdin::WriteStdinTool>()
            .is_some()
        {
            return Some(ExecKind::Stdin);
        }
        return None;
    }
    if call.origin().namespace().is_none() {
        match call.origin().name() {
            "exec_command" => return Some(ExecKind::Command),
            "write_stdin" => return Some(ExecKind::Stdin),
            _ => {}
        }
    }
    None
}

/// Whether `call` selected the patch tool, as Codex chooses the hook payload by the selected
/// handler. A host may register an ordinary function, or an MCP tool, under the same name; those
/// are hooked as the functions they are. Without the selected executable, only a bare
/// `apply_patch` name can mean the patch tool.
fn is_patch_tool(call: &ToolHookData<'_>) -> bool {
    if let Some(tool) = call.tool() {
        return (tool as &dyn std::any::Any)
            .downcast_ref::<ra_tools::apply_patch::ApplyPatchTool>()
            .is_some();
    }
    call.origin().namespace().is_none() && call.origin().name() == "apply_patch"
}

/// The source's hook-facing tool identity: the serialized name and extra matcher inputs.
#[derive(Debug, Clone, PartialEq, Eq)]
struct HookToolName {
    name: String,
    matcher_aliases: Vec<String>,
}

impl HookToolName {
    fn for_call(call: &ToolHookData<'_>) -> Self {
        if let Some(tool) = call.tool()
            && let Some(name) = crate::mcp::hook_tool_name(tool)
        {
            return Self {
                name: name.to_owned(),
                matcher_aliases: Vec::new(),
            };
        }
        if exec_kind(call) == Some(ExecKind::Command) {
            return Self {
                name: "Bash".to_owned(),
                matcher_aliases: Vec::new(),
            };
        }
        // `Write` and `Edit` select the same handlers for edit-oriented hook configurations.
        if is_patch_tool(call) {
            return Self {
                name: "apply_patch".to_owned(),
                matcher_aliases: vec!["Write".to_owned(), "Edit".to_owned()],
            };
        }
        let origin = call.origin();
        let namespace = origin.namespace().map(ra_core::tool::ToolNamespace::as_str);
        match origin.name() {
            // Shell-like tools keep the historical hook name.
            "exec_command" | "shell" | "shell_command" | "local_shell"
                if call.tool().is_none() && origin.namespace().is_none() =>
            {
                Self {
                    name: "Bash".to_owned(),
                    matcher_aliases: Vec::new(),
                }
            }
            // Codex's `function_hook_tool_name`: the `Agent` alias holds in the default namespace
            // and in `multi_agent_v1`, not in other namespaces such as `collaboration`.
            "spawn_agent"
                if is_default_namespace(namespace)
                    || namespace == Some(MULTI_AGENT_V1_NAMESPACE) =>
            {
                Self {
                    name: "spawn_agent".to_owned(),
                    matcher_aliases: vec!["Agent".to_owned()],
                }
            }
            name => Self {
                name: flat_tool_name(namespace, name),
                matcher_aliases: Vec::new(),
            },
        }
    }
}

/// Codex's `DEFAULT_FUNCTION_NAMESPACE`, which names a tool as if it had no namespace.
const DEFAULT_FUNCTION_NAMESPACE: &str = "functions";

/// The namespace of Codex's first multi-agent tools, whose `spawn_agent` keeps the `Agent` alias.
const MULTI_AGENT_V1_NAMESPACE: &str = "multi_agent_v1";

/// Codex's `ToolName::is_default_namespace`: no namespace, an empty one, or `functions`.
fn is_default_namespace(namespace: Option<&str>) -> bool {
    matches!(namespace, None | Some("" | DEFAULT_FUNCTION_NAMESPACE))
}

/// Codex's `flat_tool_name`, the hook-facing name of a function tool: the bare name in the
/// default namespace, otherwise the namespace and the name joined with nothing between them, so
/// `collaboration` and `spawn_agent` give `collaborationspawn_agent`. It is not the origin's
/// qualified name (`collaboration.spawn_agent`), which registries and persisted state key on.
fn flat_tool_name(namespace: Option<&str>, name: &str) -> String {
    match namespace {
        Some(namespace) if !is_default_namespace(Some(namespace)) => format!("{namespace}{name}"),
        _ => name.to_owned(),
    }
}

fn blocked_tool_message(tool: &HookToolName, input: &Value, reason: &str) -> String {
    if (tool.name == "Bash" || tool.name == "apply_patch")
        && let Some(command) = input.get("command").and_then(Value::as_str)
    {
        format!("Command blocked by PreToolUse hook: {reason}. Command: {command}")
    } else {
        format!(
            "Tool call blocked by PreToolUse hook: {reason}. Tool: {}",
            tool.name
        )
    }
}

fn hook_permission_mode(approval_policy: AskForApproval) -> String {
    // Every policy that may still ask the user maps to the source's `default` label.
    if matches!(approval_policy, AskForApproval::Never) {
        "bypassPermissions"
    } else {
        "default"
    }
    .to_string()
}

fn user_prompt(input: &[ModelInputItem]) -> String {
    input
        .iter()
        .filter_map(|item| match item {
            ModelInputItem::Message(message) if message.role() == MessageRole::User => {
                Some(message.text_content())
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Where hook context goes: the current turn's queue, or, between turns, the next one's.
#[derive(Default)]
struct ContextDelivery {
    /// The current turn's context queue: its root run's.
    root: Option<HookContextQueue>,
    /// Context and background warnings no open turn took yet, for the next turn.
    pending: Vec<Message>,
    pending_warnings: Vec<String>,
    /// The turn each bound run belongs to, by run identity.
    turn_of: std::collections::HashMap<String, String>,
}

impl ContextDelivery {
    /// Codex drains background results into the current turn whichever turn started them.
    fn deliver_background(&mut self, messages: Vec<Message>, warnings: Vec<String>) {
        if messages.is_empty() && warnings.is_empty() {
            return;
        }
        let (messages, warnings) = match &self.root {
            Some(queue) => match queue.push_background_with_warnings(messages, warnings) {
                Ok(()) => return,
                Err(rejected) => rejected,
            },
            None => (messages, warnings),
        };
        self.pending.extend(messages);
        self.pending_warnings.extend(warnings);
    }

    fn deliver_pending(&mut self) {
        let messages = std::mem::take(&mut self.pending);
        let warnings = std::mem::take(&mut self.pending_warnings);
        self.deliver_background(messages, warnings);
    }

    fn discard_pending(&mut self) {
        self.pending.clear();
        self.pending_warnings.clear();
    }
}

/// Whether `call` is an `apply_patch` call whose patch text cannot be read or parsed.
fn is_unparsable_patch(call: &ToolHookData<'_>) -> bool {
    // Another tool that happens to be named `apply_patch` is not the patch tool.
    if !is_patch_tool(call) {
        return false;
    }
    // Argument text that was not JSON is never a patch, even when it reads as one: Codex gives its
    // hooks a patch only from freeform input, and such a call is a function call.
    if call.has_unparsed_arguments() {
        return true;
    }
    let patch = match call.arguments() {
        Value::String(patch) => Some(patch.as_str()),
        arguments => arguments.get("patch").and_then(Value::as_str),
    };
    patch.is_none_or(|patch| ra_patch::parse_patch(patch).is_err())
}

fn hook_input(call: &ToolHookData<'_>) -> Value {
    if call.tool().and_then(crate::mcp::hook_tool_name).is_some() {
        return call.arguments().clone();
    }
    if exec_kind(call) == Some(ExecKind::Command) {
        return serde_json::json!({"command": call.arguments().get("cmd").cloned().unwrap_or(Value::Null)});
    }
    if is_patch_tool(call) {
        return serde_json::json!({"command": call.arguments().as_str().map(str::to_owned).or_else(|| call.arguments().get("patch").and_then(Value::as_str).map(str::to_owned))});
    }
    call.arguments().clone()
}
/// Codex's `PermissionRequestPayload` input: exec carries the model's `justification` as
/// `description` whenever it was given, an empty one included; terminal input is described by the
/// approval `write_stdin` prepared, falling back to the call's arguments when the hooks are asked
/// with none (a fixed `ask` rule over input the policy would not have asked about) or with one this
/// product cannot read; every other tool is unchanged.
fn permission_input(call: &ToolHookData<'_>) -> Value {
    if exec_kind(call) == Some(ExecKind::Stdin)
        && let Some(input) = call.approval_state().and_then(|state| {
            crate::thread_settings::stdin_permission_input(state, call.call_id().as_str())
        })
    {
        return input;
    }
    let mut input = hook_input(call);
    if exec_kind(call) == Some(ExecKind::Command)
        && let Some(description) = call
            .arguments()
            .get("justification")
            .and_then(Value::as_str)
        && let Some(object) = input.as_object_mut()
    {
        object.insert(
            "description".to_owned(),
            Value::String(description.to_owned()),
        );
    }
    input
}

/// Codex's permission-request run id: a retry outside the sandbox is asked under
/// `{call_id}:retry`, every other approval under the call's id.
fn permission_run_id_suffix(call: &ToolHookData<'_>) -> String {
    let retry = exec_kind(call) == Some(ExecKind::Command)
        && call
            .approval_state()
            .is_some_and(crate::thread_settings::is_sandbox_retry_approval);
    if retry {
        format!("{}:retry", call.call_id())
    } else {
        call.call_id().to_string()
    }
}
fn native_input(call: &ToolHookData<'_>, input: Value) -> std::result::Result<Value, String> {
    if call.tool().and_then(crate::mcp::hook_tool_name).is_some() {
        return Ok(input);
    }
    let patch = is_patch_tool(call);
    if patch || exec_kind(call) == Some(ExecKind::Command) {
        let command = input
            .get("command")
            .and_then(Value::as_str)
            .ok_or_else(|| "hook input rewrite requires a string command".to_owned())?;
        if patch {
            return match call.arguments() {
                Value::String(_) => Ok(Value::String(command.to_owned())),
                Value::Object(_) => {
                    let mut native = call.arguments().clone();
                    native["patch"] = Value::String(command.to_owned());
                    Ok(native)
                }
                _ => Err(
                    "hook input rewrite requires string or object apply_patch arguments".to_owned(),
                ),
            };
        }
        let mut native = call.arguments().clone();
        native
            .as_object_mut()
            .ok_or_else(|| "hook input rewrite requires exec_command arguments".to_owned())?
            .insert("cmd".to_owned(), Value::String(command.to_owned()));
        return Ok(native);
    }
    Ok(input)
}
fn escape_xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
