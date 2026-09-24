//! The runner's view of sandbox execution: prepare an agent before its turn, clean up at the end.
//!
//! Ported from the reference's `sandbox/runtime.py`. One of these exists per run. It holds the
//! session manager and a cache of prepared agents, so a sandbox agent is assembled once per session
//! rather than once per turn.

use std::{
    collections::BTreeMap,
    fmt,
    sync::{Arc, Mutex as StdMutex, MutexGuard, PoisonError},
};

use ra_core::{
    agent::{AgentId, AgentSpec},
    capability::SandboxBinding,
    error::{Error, Result},
    sandbox::{SandboxSession, SandboxWorkspaceScope},
};
use serde_json::Value;
use tokio::sync::{Mutex, oneshot, watch};
use tracing::{Instrument, warn};

use crate::agent::AgentBinding;

use super::{
    SandboxRunConfig,
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
}

struct Inner {
    manager: SessionManager,
    prepared: BTreeMap<AgentId, PreparedAgent>,
}

/// Sandbox execution for one run.
pub(crate) struct SandboxRuntime {
    workspace_scope: SandboxWorkspaceScope,
    /// `None` when the run has no sandbox configuration, which is a run that refuses sandbox
    /// agents rather than one that ignores them.
    inner: Option<Mutex<Inner>>,
    /// Set once cleanup has started; turns `true` when it has finished.
    cleanup_done: StdMutex<Option<watch::Receiver<bool>>>,
}

impl SandboxRuntime {
    /// Sandbox execution for a run configured with `config`, continued from `resumed` if the run's
    /// checkpoint carried a resume payload.
    pub(crate) fn new(config: Option<SandboxRunConfig>, resumed: Option<Value>) -> Self {
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
            inner: config.map(|config| {
                Mutex::new(Inner {
                    manager: SessionManager::new(config, resumed),
                    prepared: BTreeMap::new(),
                })
            }),
            cleanup_done: StdMutex::new(None),
        }
    }

    /// Whether the run has a sandbox configuration.
    pub(crate) const fn enabled(&self) -> bool {
        self.inner.is_some()
    }

    /// Refuses a sandbox agent in a run that has no sandbox configuration.
    pub(crate) fn assert_agent_supported(&self, agent: &AgentSpec) -> Result<()> {
        if agent.sandbox().is_some() && !self.enabled() {
            return Err(Error::config(
                "SandboxAgent execution requires `RunConfig(sandbox=...)`",
            ));
        }
        Ok(())
    }

    /// The binding a turn runs `agent` under.
    ///
    /// An ordinary agent comes back unchanged. A sandbox agent comes back prepared against its
    /// session — created, resumed or borrowed the first time, and started when it is not running —
    /// with the cached preparation reused while the session is the same one.
    pub(crate) async fn prepare_agent(&self, agent: &AgentBinding) -> Result<AgentBinding> {
        let public = agent.public();
        self.assert_agent_supported(public)?;
        let (Some(sandbox), Some(inner)) = (public.sandbox(), &self.inner) else {
            return Ok(agent.clone());
        };
        let span = tracing::info_span!("sandbox.prepare_agent", agent.name = %public.name());
        async {
            let mut inner = inner.lock().await;
            inner.manager.acquire_agent(public, sandbox)?;
            let session = inner.manager.ensure_session(public, sandbox).await?;
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
            )
            .await?;
            inner.prepared.insert(
                public.id().clone(),
                PreparedAgent {
                    base,
                    agent: Arc::clone(&prepared),
                    session,
                },
            );
            Ok(AgentBinding::prepared(Arc::clone(public), prepared))
        }
        .instrument(span)
        .await
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
