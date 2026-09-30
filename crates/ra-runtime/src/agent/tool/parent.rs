//! What a nested run inherits from the run whose tool call started it.
//!
//! The reference hands an agent tool the parent's run configuration through its tool context, and
//! the nested run falls back to it when the tool was given none. Here that context is
//! [`ToolContext`](ra_core::tool::ToolContext), which lives in `ra-core` and cannot name
//! [`RunConfig`] or the model resolver without reversing the crate dependency. So the runner keeps
//! the parent's execution environment in a task-local for the duration of its loop, carries it
//! across the task spawn each tool dispatch makes, and narrows it to the call's own cancellation
//! scope immediately around [`Tool::call`](ra_core::tool::Tool::call).
//!
//! Nothing outside this crate can read it. A tool invoked directly — not through the runner's
//! dispatch — finds no parent here, which [`super::AgentTool`] reports as a caller error rather
//! than guessing a model provider.
//!
//! The narrowing also carries, for a call the parent is resuming, the checkpoint of the nested run
//! that call paused on. The reference finds that state in its registry by tool-call identity; here
//! the parent's own state holds it, and the runner hands it to exactly the call it belongs to. A
//! nested run installs its own environment around its loop, so the checkpoint never reaches a call
//! the nested run makes in turn.

use std::{any::Any, future::Future, sync::Arc};

use ra_core::{cancel::CancelScope, model::ModelResolver, state::RunState};

use crate::runner::RunConfig;

tokio::task_local! {
    static CURRENT: ParentRun;
}

/// The parent run's execution environment, as a nested run starts from it.
#[derive(Clone)]
pub(crate) struct ParentRun {
    run: Arc<ParentRunEnvironment>,
    call_scope: Option<CancelScope>,
    resume: Option<Arc<RunState>>,
}

struct ParentRunEnvironment {
    model_resolver: Arc<dyn ModelResolver>,
    config: RunConfig,
    app_context: Option<Arc<dyn Any + Send + Sync>>,
}

impl ParentRun {
    /// Captures a run's environment before its loop consumes the request.
    pub(crate) fn new(
        model_resolver: Arc<dyn ModelResolver>,
        config: RunConfig,
        app_context: Option<Arc<dyn Any + Send + Sync>>,
    ) -> Self {
        Self {
            run: Arc::new(ParentRunEnvironment {
                model_resolver,
                config,
                app_context,
            }),
            call_scope: None,
            resume: None,
        }
    }

    /// The environment of the run whose code is executing, if a runner installed one.
    pub(crate) fn current() -> Option<Self> {
        CURRENT.try_with(Clone::clone).ok()
    }

    /// Runs `future` as code belonging to this run.
    pub(crate) async fn scope<F: Future>(self, future: F) -> F::Output {
        CURRENT.scope(self, future).await
    }

    /// Narrows the environment to one tool call: its cancellation scope, and the paused nested run
    /// it continues, if it is being resumed.
    #[must_use]
    pub(crate) fn for_call(
        mut self,
        call_scope: CancelScope,
        resume: Option<Arc<RunState>>,
    ) -> Self {
        self.call_scope = Some(call_scope);
        self.resume = resume;
        self
    }

    pub(crate) fn model_resolver(&self) -> &Arc<dyn ModelResolver> {
        &self.run.model_resolver
    }

    pub(crate) fn config(&self) -> &RunConfig {
        &self.run.config
    }

    pub(crate) fn app_context(&self) -> Option<&Arc<dyn Any + Send + Sync>> {
        self.run.app_context.as_ref()
    }

    /// The scope of the tool call being executed; absent outside a dispatched call.
    pub(crate) const fn call_scope(&self) -> Option<&CancelScope> {
        self.call_scope.as_ref()
    }

    /// The paused nested run the call being executed continues; absent for a fresh call.
    pub(crate) fn nested_resume(&self) -> Option<&RunState> {
        self.resume.as_deref()
    }
}

/// Carries the current run's environment into a future that is about to be spawned.
///
/// A task-local does not follow a spawn, and the batch executor spawns every tool dispatch; without
/// this, a nested agent called from a batch would find no parent at all.
pub(crate) fn carry<F>(future: F) -> impl Future<Output = F::Output>
where
    F: Future,
{
    let parent = ParentRun::current();
    async move {
        match parent {
            Some(parent) => parent.scope(future).await,
            None => future.await,
        }
    }
}

/// Runs a tool invocation narrowed to its own call, when it belongs to a run.
pub(crate) async fn within_call<F: Future>(
    call_scope: &CancelScope,
    resume: Option<Arc<RunState>>,
    future: F,
) -> F::Output {
    match ParentRun::current() {
        Some(parent) => {
            parent
                .for_call(call_scope.clone(), resume)
                .scope(future)
                .await
        }
        None => future.await,
    }
}
