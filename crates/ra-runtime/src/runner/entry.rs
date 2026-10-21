//! Input conversion and configured entry points over the existing runner.

use std::{cell::RefCell, sync::Arc};

use ra_core::{
    cancel::CancelScope,
    error::{Error, Result},
    item::{Message, ModelInputItem, RunItem},
    model::ModelResolver,
    session::SessionInputCallback,
    state::{RunId, RunState},
};
use tokio::runtime::Runtime;

use super::{RunConfig, RunRequest, RunResult, RunStream, Runner};
use crate::agent::AgentBinding;

/// Initial user text, typed model input, or a checkpoint to continue.
///
/// Text becomes one user message. Item order and duplicates are preserved. A checkpoint keeps
/// its run identity and uses the existing automatic history projection; it never starts a new run.
#[non_exhaustive]
pub enum RunInput {
    /// One user message, including empty text when explicitly supplied.
    Text(String),
    /// Caller-supplied model input, without rewriting or deduplication.
    Items(Vec<ModelInputItem>),
    /// An owned checkpoint. Boxed because it is much larger than initial input.
    State(Box<RunState>),
}

impl From<&str> for RunInput {
    fn from(text: &str) -> Self {
        Self::Text(text.to_owned())
    }
}

impl From<String> for RunInput {
    fn from(text: String) -> Self {
        Self::Text(text)
    }
}

impl From<&String> for RunInput {
    fn from(text: &String) -> Self {
        Self::Text(text.clone())
    }
}

impl From<ModelInputItem> for RunInput {
    fn from(item: ModelInputItem) -> Self {
        Self::Items(vec![item])
    }
}

impl From<Vec<ModelInputItem>> for RunInput {
    fn from(items: Vec<ModelInputItem>) -> Self {
        Self::Items(items)
    }
}

impl<const N: usize> From<[ModelInputItem; N]> for RunInput {
    fn from(items: [ModelInputItem; N]) -> Self {
        Self::Items(Vec::from(items))
    }
}

impl From<RunState> for RunInput {
    fn from(state: RunState) -> Self {
        Self::State(Box::new(state))
    }
}

/// A reusable runner with an explicitly configured model resolver.
///
/// A shipped provider registry implements that contract. Configure it once, then use
/// `runner.run(agent, "hello").await`. For session, context, cancellation, or other per-run
/// options, use `runner.request(agent, input).with_session(session).run().await`.
///
/// This is the explicit-provider counterpart of Python's `AgentRunner`. It owns no current agent,
/// conversation, or mutable run state, so calls can safely share it. All execution is delegated to
/// [`Runner`], including approvals, handoffs, output validation, persistence, and recovery.
#[derive(Clone)]
pub struct AgentRunner {
    model_resolver: Arc<dyn ModelResolver>,
}

impl AgentRunner {
    /// Configures the resolver reused by every call, without creating a global default client.
    #[must_use]
    pub fn new(model_resolver: Arc<dyn ModelResolver>) -> Self {
        Self { model_resolver }
    }

    /// Builds an owned request with default run configuration and a fresh cancellation scope.
    ///
    /// New input receives a new identity; checkpoint input retains its persisted identity. The
    /// existing request builders remain available, including explicit state and rollout recovery.
    pub fn request(
        &self,
        agent: impl Into<AgentBinding>,
        input: impl Into<RunInput>,
    ) -> RunRequest {
        let (input, state) = match input.into() {
            RunInput::Text(text) => (vec![ModelInputItem::Message(Message::user(text))], None),
            RunInput::Items(items) => (items, None),
            RunInput::State(state) => (Vec::new(), Some(*state)),
        };
        let run_id = state
            .as_ref()
            .map_or_else(RunId::generate, |state| state.run_id().clone());
        let request = RunRequest::new(
            agent.into(),
            Arc::clone(&self.model_resolver),
            run_id,
            CancelScope::root(),
            input,
        );
        match state {
            Some(state) => request.with_state(state),
            None => request,
        }
    }

    /// Runs initial input or a checkpoint through the existing async loop.
    pub async fn run(
        &self,
        agent: impl Into<AgentBinding>,
        input: impl Into<RunInput>,
    ) -> Result<RunResult> {
        self.request(agent, input).run().await
    }

    /// Runs synchronously, reusing this thread's library-owned Tokio runtime.
    ///
    /// Returns a caller error when entered from a Tokio runtime; use [`Self::run`] there.
    pub fn run_sync(
        &self,
        agent: impl Into<AgentBinding>,
        input: impl Into<RunInput>,
    ) -> Result<RunResult> {
        self.request(agent, input).run_sync()
    }

    /// Starts a streamed run on the current Tokio runtime, with the existing drop cancellation.
    ///
    /// Requires a Tokio runtime with its time driver enabled, as [`Runner::run_streamed`] does.
    #[must_use]
    pub fn run_streamed(
        &self,
        agent: impl Into<AgentBinding>,
        input: impl Into<RunInput>,
    ) -> RunStream {
        self.request(agent, input).run_streamed()
    }
}

thread_local! {
    // A current-thread runtime, like Python's default loop, remains alive between sync calls.
    // Provider pools and session primitives can consequently be reused on the same executor.
    static SYNC_RUNTIME: RefCell<Option<Runtime>> = const { RefCell::new(None) };
}

/// Drives the run `start` builds on `runtime` from a scoped thread with Codex's stack budget.
///
/// The runtime stays the calling thread's: a current-thread runtime may be entered from any
/// thread, one at a time, and the scope joins before the caller's borrow of it ends. The future
/// is built on the new thread, so it need not be `Send`. The caller's tracing dispatcher and
/// current span are installed there first, because a thread-local default subscriber and the
/// current-span stack would otherwise stay behind on the calling thread.
fn block_on_with_stack_budget<F, Fut>(runtime: &Runtime, start: F) -> Result<RunResult>
where
    F: FnOnce() -> Fut + Send,
    Fut: std::future::Future<Output = Result<RunResult>>,
{
    let dispatch = tracing::dispatcher::get_default(Clone::clone);
    let span = tracing::Span::current();
    std::thread::scope(|scope| {
        let worker = std::thread::Builder::new()
            .name("ra-sync-run".to_owned())
            .stack_size(super::THREAD_STACK_SIZE_BYTES)
            .spawn_scoped(scope, move || {
                tracing::dispatcher::with_default(&dispatch, || {
                    let _entered = span.enter();
                    runtime.block_on(start())
                })
            })
            .map_err(|error| {
                Error::caller("failed to start the sync run thread").with_source(error)
            })?;
        worker
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
    })
}

impl Runner {
    /// Runs an explicit request synchronously outside a Tokio runtime.
    ///
    /// The per-thread current-thread runtime has IO and time enabled and is reused between calls.
    /// It only progresses while this thread is inside `run_sync`. Hosts needing background work
    /// between calls should own a Tokio runtime and await [`Self::run`] instead. No signal handler
    /// is installed; a host can cancel from another thread using the request's explicit scope.
    ///
    /// The run is polled on a library-owned thread with
    /// [`super::THREAD_STACK_SIZE_BYTES`] of stack, started for the call and joined before it
    /// returns, as Codex runs its entry point on a thread with that budget instead of the
    /// caller's stack. Nested agent runs need it in debug builds; the caller's thread may have a
    /// far smaller stack. The caller's tracing dispatcher and current span are carried over, so
    /// the run's spans reach the same subscriber with the same parent. Other thread-local state
    /// of the caller is not visible to the run's callbacks. A panic in the run is resumed on the
    /// caller.
    pub fn run_sync(request: RunRequest) -> Result<RunResult> {
        if tokio::runtime::Handle::try_current().is_ok() {
            return Err(Error::caller(
                "Runner::run_sync cannot be called inside a Tokio runtime; await Runner::run instead",
            ));
        }
        SYNC_RUNTIME.with(|slot| {
            let mut runtime = slot.borrow_mut();
            if runtime.is_none() {
                *runtime = Some(
                    tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .map_err(|error| {
                            Error::caller("failed to create the sync run runtime")
                                .with_source(error)
                        })?,
                );
            }
            match runtime.as_ref() {
                Some(runtime) => block_on_with_stack_budget(runtime, move || Self::run(request)),
                None => Err(Error::caller("the sync run runtime was not initialized")),
            }
        })
    }
}

impl RunRequest {
    /// Uses a host-owned cancellation scope, including propagation from its parent.
    pub fn with_cancel_scope(mut self, cancel: CancelScope) -> Self {
        self.cancel = cancel;
        self
    }

    /// Attaches an owned application context, using the existing checked typed access.
    ///
    /// The value is stored as `T` itself, so code reads it with `app_context::<T>()`. Do not pass
    /// an `Arc<T>` here: it would be stored as `Arc<T>`, and `app_context::<T>()` would then return
    /// `None` just as it does for a missing context. Pass an already shared object to
    /// [`Self::with_app_context`] instead.
    pub fn with_context<T: Send + Sync + 'static>(self, context: T) -> Self {
        self.with_app_context(Arc::new(context))
    }

    /// Executes this request with [`Runner::run`].
    pub async fn run(self) -> Result<RunResult> {
        Runner::run(self).await
    }

    /// Executes this request with [`Runner::run_sync`].
    pub fn run_sync(self) -> Result<RunResult> {
        Runner::run_sync(self)
    }

    /// Executes this request with [`Runner::run_streamed`].
    ///
    /// Requires a Tokio runtime with its time driver enabled.
    #[must_use]
    pub fn run_streamed(self) -> RunStream {
        Runner::run_streamed(self)
    }
}

struct AsyncSessionInputCallback<F>(F);

#[async_trait::async_trait]
impl<F> SessionInputCallback for AsyncSessionInputCallback<F>
where
    F: for<'a> Fn(
            &'a mut Vec<RunItem>,
            &'a mut Vec<RunItem>,
        ) -> futures::future::BoxFuture<'a, Result<Vec<RunItem>>>
        + Send
        + Sync,
{
    async fn combine(
        &self,
        history: &mut Vec<RunItem>,
        new_input: &mut Vec<RunItem>,
    ) -> Result<Vec<RunItem>> {
        (self.0)(history, new_input).await
    }
}

impl RunConfig {
    /// Installs an asynchronous session merge closure that may borrow and mutate both lists.
    ///
    /// Return `Box::pin(async move { ... })`. Mutation, item ownership, errors, and cancellation
    /// use the existing session callback contract. Synchronous closures already implement that
    /// trait and can be installed with [`Self::with_session_input_callback`].
    pub fn with_session_input_callback_async_fn<F>(self, callback: F) -> Self
    where
        F: for<'a> Fn(
                &'a mut Vec<RunItem>,
                &'a mut Vec<RunItem>,
            ) -> futures::future::BoxFuture<'a, Result<Vec<RunItem>>>
            + Send
            + Sync
            + 'static,
    {
        self.with_session_input_callback(Arc::new(AsyncSessionInputCallback(callback)))
    }
}
