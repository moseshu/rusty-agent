//! The runner's view of sandbox execution: prepare an agent before its turn, clean up at the end.
//!
//! Ported from the reference's `sandbox/runtime.py`. One of these exists per run. It holds the
//! session manager and a cache of prepared agents, so a sandbox agent is assembled once per session
//! rather than once per turn, and it records the run for sandbox memory when the agent it last
//! prepared generates memory.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    sync::{Arc, Mutex as StdMutex, MutexGuard, PoisonError},
};

use ra_core::{
    agent::{AgentId, AgentSpec},
    capability::{Capability, CapabilityFamily, ContextProcessor, SamplingContext, SandboxBinding},
    error::{Error, Result},
    item::{ModelInputItem, RunItem},
    model::ModelResolver,
    sandbox::{SandboxMemory, SandboxSession, SandboxWorkspaceScope},
};
use serde_json::Value;
use tokio::sync::{Mutex, oneshot, watch};
use tracing::{Instrument, warn};

use crate::agent::AgentBinding;
use crate::capability::CapabilityContextProcessor;
use crate::runner::RunResult;

use super::{
    SandboxRunConfig,
    memory::{
        manager::{SandboxMemoryGenerationManager, get_or_create_memory_generation_manager},
        rollouts::{build_rollout_payload, terminal_metadata_for_error},
    },
    preparation::{bind_capabilities, prepare_sandbox_agent, validate_workspace_scope},
    session_manager::SessionManager,
};

/// One prepared sandbox agent, valid while its session is the one it was prepared against.
struct PreparedAgent {
    /// What it was prepared from, so a later preparation starts from the same instance rather
    /// than wrapping an already prepared one a second time.
    base: Arc<AgentSpec>,
    agent: Arc<AgentSpec>,
    session: Arc<dyn SandboxSession>,
    /// The capabilities as bound to that session, whose context processing runs on the agent's
    /// turns.
    capabilities: Vec<Arc<dyn Capability>>,
}

struct Inner {
    manager: SessionManager,
    prepared: BTreeMap<AgentId, PreparedAgent>,
    /// The session of the agent prepared last, the reference's `current_session`.
    current_session: Option<Arc<dyn SandboxSession>>,
}

/// What a failed run got through, for sandbox memory.
struct FailedSegment {
    input: Vec<ModelInputItem>,
    items: Vec<RunItem>,
}

/// Sandbox execution for one run.
pub(crate) struct SandboxRuntime {
    workspace_scope: SandboxWorkspaceScope,
    /// The families of the capabilities the run installs for every agent, which a sandbox agent's
    /// own capabilities must not claim again.
    run_capability_families: BTreeSet<CapabilityFamily>,
    /// `None` when the run has no sandbox configuration, which is a run that refuses sandbox
    /// agents rather than one that ignores them.
    inner: Option<Mutex<Inner>>,
    /// Set once cleanup has started; turns `true` when it has finished.
    cleanup_done: StdMutex<Option<watch::Receiver<bool>>>,
    /// Resolves the models of sandbox memory's own runs.
    model_resolver: Arc<dyn ModelResolver>,
    /// The rollout this run's segment is recorded under; `None` without a sandbox configuration.
    rollout_id: Option<String>,
    /// The memory capability of the agent prepared last, the reference's
    /// `_active_memory_capability`.
    active_memory: StdMutex<Option<SandboxMemory>>,
    /// Set by the runner when the run fails after its turns started.
    failed_segment: StdMutex<Option<FailedSegment>>,
}

impl SandboxRuntime {
    /// Sandbox execution for a run configured with `config`, continued from `resumed` if the run's
    /// checkpoint carried a resume payload.
    ///
    /// `run_capabilities` are the capabilities the run installs for every agent, `model_resolver`
    /// resolves the models sandbox memory runs with, and `rollout_id` names the rollout the run is
    /// recorded under.
    pub(crate) fn new(
        config: Option<SandboxRunConfig>,
        resumed: Option<Value>,
        run_capabilities: &[Arc<dyn Capability>],
        model_resolver: Arc<dyn ModelResolver>,
        rollout_id: Option<String>,
    ) -> Self {
        let workspace_scope = SandboxWorkspaceScope::from_cwd(
            config
                .as_ref()
                .and_then(SandboxRunConfig::cwd)
                .map(ra_core::sandbox::PosixPath::as_str),
        )
        // The configuration normalized its working directory when it was set, so a second
        // normalization of the same value cannot refuse it.
        .unwrap_or_default();
        Self {
            workspace_scope,
            run_capability_families: run_capabilities
                .iter()
                .map(|capability| capability.kind())
                .collect(),
            inner: config.map(|config| {
                Mutex::new(Inner {
                    manager: SessionManager::new(config, resumed),
                    prepared: BTreeMap::new(),
                    current_session: None,
                })
            }),
            cleanup_done: StdMutex::new(None),
            model_resolver,
            rollout_id,
            active_memory: StdMutex::new(None),
            failed_segment: StdMutex::new(None),
        }
    }

    /// Whether the run has a sandbox configuration.
    pub(crate) const fn enabled(&self) -> bool {
        self.inner.is_some()
    }

    /// Refuses a sandbox agent in a run that has no sandbox configuration, and one whose own
    /// capabilities claim a family the run already installs for every agent.
    ///
    /// The second refusal has no counterpart in the reference, which installs capabilities only on
    /// a sandbox agent. Here a run can also install them for every agent, and the built-in set for
    /// that route shares its family names with the sandbox set while meaning something else: the
    /// run-level `shell` and `filesystem` act on the host's workspace, the sandbox ones on the
    /// session's; the run-level `memory` reads a store, the sandbox one the session's files; the
    /// run-level `compaction` summarizes locally, the sandbox one asks the provider. Installed
    /// together, one agent would hold two meanings under one name, so the run is refused before a
    /// session exists rather than left to whichever tool the model happens to call. The check
    /// covers every sandbox agent the run prepares, a handoff target included: run-level tools and
    /// prompt text reach the starting agent only, but run-level context processing runs on every
    /// turn.
    pub(crate) fn assert_agent_supported(&self, agent: &AgentSpec) -> Result<()> {
        let Some(sandbox) = agent.sandbox() else {
            return Ok(());
        };
        if !self.enabled() {
            return Err(Error::config(
                "SandboxAgent execution requires `RunConfig(sandbox=...)`",
            ));
        }
        let shared: BTreeSet<CapabilityFamily> = sandbox
            .capabilities()
            .iter()
            .map(|capability| capability.kind())
            .filter(|family| self.run_capability_families.contains(family))
            .collect();
        if !shared.is_empty() {
            let families = shared
                .iter()
                .map(|family| format!("`{family}`"))
                .collect::<Vec<_>>()
                .join(", ");
            return Err(Error::config(format!(
                "sandbox agent `{}` installs capability families {families} that the run also \
                 installs for every agent; a run-level capability and a sandbox capability of one \
                 family are two different capabilities under one name, so the agent would hold \
                 both meanings at once. Install each family in one place: remove it from the \
                 run's capabilities or from the sandbox agent's",
                agent.id()
            )));
        }
        Ok(())
    }

    /// The binding a turn runs `agent` under.
    ///
    /// An ordinary agent comes back unchanged. A sandbox agent comes back prepared against its
    /// session — created, resumed or borrowed the first time, and started when it is not running —
    /// with the cached preparation reused while the session is the same one.
    ///
    /// The capabilities' sampling settings are folded for the model the turn will use, resolved the
    /// way turn preparation resolves it — `model_override`, else the agent's own model — as the
    /// reference's `resolve_sandbox_model_name` does.
    pub(crate) async fn prepare_agent(
        &self,
        agent: &AgentBinding,
        resolver: &dyn ModelResolver,
        model_override: Option<&str>,
    ) -> Result<AgentBinding> {
        let public = agent.public();
        self.assert_agent_supported(public)?;
        // For every agent, a sandbox agent or not: a handoff to an agent without memory stops the
        // run being recorded, and one to an agent with memory starts it.
        *lock(&self.active_memory) = public.sandbox().and_then(|sandbox| {
            sandbox
                .capabilities()
                .iter()
                .find_map(|capability| capability.sandbox_memory())
        });
        let (Some(sandbox), Some(inner)) = (public.sandbox(), &self.inner) else {
            return Ok(agent.clone());
        };
        let span = tracing::info_span!("sandbox.prepare_agent", agent.name = %public.name());
        async {
            let mut inner = inner.lock().await;
            inner.manager.acquire_agent(public, sandbox)?;
            let session = inner.manager.ensure_session(public, sandbox).await?;
            inner.current_session = Some(Arc::clone(&session));
            validate_workspace_scope(session.as_ref(), &self.workspace_scope, sandbox.run_as())
                .await?;

            let base = match inner.prepared.get(public.id()) {
                Some(cached) if Arc::ptr_eq(&cached.session, &session) => {
                    return Ok(AgentBinding::prepared(
                        Arc::clone(public),
                        Arc::clone(&cached.agent),
                    ));
                }
                Some(cached) => Arc::clone(&cached.base),
                None => Arc::clone(agent.execution()),
            };

            let selector = resolver
                .resolve_model(model_override.or(base.model()))?
                .selector()
                .clone();
            let mut sampling = SamplingContext::new().with_provider(selector.provider().clone());
            if let Some(model) = selector.model() {
                sampling = sampling.with_model(model);
            }

            let manifest = session.state().manifest().clone();
            let binding = SandboxBinding::new(
                Arc::clone(&session),
                sandbox.run_as().cloned(),
                self.workspace_scope.clone(),
                manifest.clone(),
            );
            let capabilities = bind_capabilities(sandbox.capabilities(), &binding)?;
            let prepared = prepare_sandbox_agent(
                &base,
                sandbox,
                &capabilities,
                &manifest,
                &self.workspace_scope,
                &sampling,
            )
            .await?;
            inner.prepared.insert(
                public.id().clone(),
                PreparedAgent {
                    base,
                    agent: Arc::clone(&prepared),
                    session,
                    capabilities,
                },
            );
            Ok(AgentBinding::prepared(Arc::clone(public), prepared))
        }
        .instrument(span)
        .await
    }

    /// The context processors `agent`'s sandbox capabilities contribute to its turns, in
    /// installation order.
    ///
    /// Empty for an ordinary agent, and for a sandbox agent not yet prepared. They run before the
    /// run's own processors, as the reference processes a sandbox agent's input while preparing it,
    /// ahead of anything else the turn does to the input.
    pub(crate) async fn context_processors(
        &self,
        agent: &AgentBinding,
    ) -> Vec<Arc<dyn ContextProcessor>> {
        let Some(inner) = &self.inner else {
            return Vec::new();
        };
        let inner = inner.lock().await;
        inner
            .prepared
            .get(agent.public().id())
            .map(|prepared| {
                prepared
                    .capabilities
                    .iter()
                    .filter(|capability| capability.context_processor().is_some())
                    .map(|capability| {
                        Arc::new(CapabilityContextProcessor::new(Arc::clone(capability)))
                            as Arc<dyn ContextProcessor>
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Records the segment a failed run got through, for [`Self::enqueue_memory_failure`].
    pub(crate) fn record_failed_segment(&self, input: &[ModelInputItem], items: &[RunItem]) {
        *lock(&self.failed_segment) = Some(FailedSegment {
            input: input.to_vec(),
            items: items.to_vec(),
        });
    }

    /// Records a finished run as a segment of its rollout, when the agent prepared last generates
    /// memory.
    ///
    /// The reference's `enqueue_memory_result`, with the run's input as the segment's input.
    pub(crate) async fn enqueue_memory_result(&self, result: &RunResult) -> Result<()> {
        let (Some(manager), Some(rollout_id)) =
            (self.memory_generation_manager().await?, &self.rollout_id)
        else {
            return Ok(());
        };
        manager.enqueue_result(result, None, rollout_id).await
    }

    /// Records a failed run as a segment of its rollout, when the agent prepared last generates
    /// memory: the records it got through, and how it failed.
    ///
    /// The reference's `enqueue_memory_payload` as its runner calls it for an exception. A run that
    /// failed before its turns started has no records, and approvals a failed turn was waiting on
    /// are not recorded.
    pub(crate) async fn enqueue_memory_failure(&self, error: &Error) -> Result<()> {
        let (Some(manager), Some(rollout_id)) =
            (self.memory_generation_manager().await?, &self.rollout_id)
        else {
            return Ok(());
        };
        let segment = lock(&self.failed_segment).take();
        let (input, items) = segment.map_or_else(
            || (Vec::new(), Vec::new()),
            |segment| (segment.input, segment.items),
        );
        let payload = build_rollout_payload(
            &input,
            &items,
            None,
            &[],
            terminal_metadata_for_error(error),
        );
        manager.enqueue_rollout_payload(payload, rollout_id).await
    }

    /// The generation manager of the current session and the active memory, if that memory
    /// generates.
    async fn memory_generation_manager(
        &self,
    ) -> Result<Option<Arc<SandboxMemoryGenerationManager>>> {
        let Some(memory) = lock(&self.active_memory)
            .clone()
            .filter(|memory| memory.generate().is_some())
        else {
            return Ok(None);
        };
        let Some(inner) = &self.inner else {
            return Ok(None);
        };
        let Some(session) = inner.lock().await.current_session.clone() else {
            return Ok(None);
        };
        get_or_create_memory_generation_manager(&session, &memory, &self.model_resolver).map(Some)
    }

    /// Cleans up the sessions the run owns and returns what resumes them.
    ///
    /// `Ok(None)` for a run whose sandbox configuration borrowed its session. Not called for a run
    /// with no sandbox configuration, whose checkpoint is left as it was.
    ///
    /// # Cleanup outlives the task that asked for it
    ///
    /// The work runs on a task of its own, and this only waits for it. A streamed run whose host
    /// walked away is aborted once its drain grace runs out, and stopping a session — persisting its
    /// workspace — can take longer than that; cleanup run on the run's own task would be cut off
    /// between stop and delete, leaving a sandbox nobody will ever release. On its own task it
    /// finishes, and [`Self::finish_cleanup`] is how the stream's reaper waits for it. A runtime to
    /// spawn on is required for that; without one the work runs here.
    pub(crate) async fn cleanup(self: &Arc<Self>) -> Result<Option<Value>> {
        if !self.enabled() {
            return Ok(None);
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return self.cleanup_now().await;
        };
        let (result_tx, result_rx) = oneshot::channel();
        {
            let mut done = lock(&self.cleanup_done);
            if done.is_some() {
                return Err(Error::caller(
                    "sandbox cleanup was already started for this run",
                ));
            }
            let (done_tx, done_rx) = watch::channel(false);
            *done = Some(done_rx);
            let runtime = Arc::clone(self);
            drop(handle.spawn(async move {
                let _ = result_tx.send(runtime.cleanup_now().await);
                let _ = done_tx.send(true);
            }));
        }
        result_rx.await.unwrap_or_else(|_| {
            Err(Error::caller(
                "sandbox cleanup ended without reporting how it went",
            ))
        })
    }

    /// Makes sure the run's sessions are released, however the run's own task ended.
    ///
    /// For the reaper of a streamed run that was aborted: the run may have been anywhere by then —
    /// before cleanup, or waiting on it. This starts cleanup if nothing did and waits for it either
    /// way. What it produced is dropped, with a warning for a failure: nobody is left to hand a
    /// result to.
    pub(crate) async fn finish_cleanup(self: &Arc<Self>) {
        if !self.enabled() {
            return;
        }
        let started = lock(&self.cleanup_done).clone();
        match started {
            Some(mut done) => {
                let _ = done.wait_for(|finished| *finished).await;
            }
            None => {
                if let Err(error) = self.cleanup().await {
                    warn!(error = %error, "failed to clean up sandbox resources after an aborted run");
                }
            }
        }
    }

    /// The cleanup itself, on whichever task calls it.
    async fn cleanup_now(&self) -> Result<Option<Value>> {
        let Some(inner) = &self.inner else {
            return Ok(None);
        };
        let mut inner = inner.lock().await;
        let span = inner
            .manager
            .has_sessions()
            .then(|| tracing::info_span!("sandbox.cleanup"));
        let result = match span {
            Some(span) => inner.manager.cleanup().instrument(span).await,
            None => inner.manager.cleanup().await,
        };
        inner.prepared.clear();
        inner.current_session = None;
        result
    }
}

impl fmt::Debug for SandboxRuntime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SandboxRuntime")
            .field("enabled", &self.enabled())
            .field("workspace_scope", &self.workspace_scope)
            .finish_non_exhaustive()
    }
}

fn lock<T>(mutex: &StdMutex<T>) -> MutexGuard<'_, T> {
    // Nothing panics while holding it, so a poisoned lock still guards a consistent value.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
