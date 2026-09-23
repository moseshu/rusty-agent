//! Session-scoped dependencies: the runtime-only objects a session's snapshot and materialization
//! reach for by key.
//!
//! A sandbox client holds a configured template of bindings and hands each session it creates or
//! resumes a copy of that template. Every session therefore gets its own factory cache and its own
//! owned-resource lifecycle, while callers still register shared objects — a storage client, a
//! lazily built service handle — once, on the client.
//!
//! # Two kinds of binding
//!
//! - **A value** is returned as bound. The container never closes it: it belongs to whoever bound
//!   it.
//! - **A factory** builds its value on demand. A cached factory runs once per container and every
//!   caller gets the same value; an uncached one runs on every resolution. A factory whose binding
//!   says it owns its result has that result closed when the container closes.
//!
//! # Tasks, not borrowed futures
//!
//! A factory runs as a spawned task rather than inside the caller's future, because that is what
//! its lifecycle rules are about. Several callers can wait on one cached factory, and one of them
//! giving up must not take the value away from the rest; a rebind or a close has to stop a factory
//! that nobody is polling. A caller waiting on an *uncached* factory is the only one who wants its
//! value, so giving up does stop that factory.
//!
//! Closing is a task for the same reason: a caller that stops waiting on a close does not stop the
//! close, and the next caller waits on the one already under way.

use std::any::Any;
use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};

use async_trait::async_trait;
use futures::future::{BoxFuture, FutureExt, Shared};
use tokio::task::AbortHandle;

/// The key a dependency is bound under.
pub type DependencyKey = String;

/// A factory's failure, shared by every caller that waited on it.
pub type DependencyFactoryError = Arc<dyn std::error::Error + Send + Sync>;

/// What a factory produces: its value, or why it could not build one.
pub type DependencyFactoryResult = Result<DependencyValue, DependencyFactoryError>;

/// A factory building one dependency. It is handed the container it was bound on, so it can resolve
/// the dependencies it needs itself.
pub type DependencyFactory =
    Arc<dyn Fn(Arc<Dependencies>) -> BoxFuture<'static, DependencyFactoryResult> + Send + Sync>;

/// Wraps an async closure as a [`DependencyFactory`].
///
/// The reference accepts any callable, sync or async; this is the Rust spelling of handing one
/// over. A factory with nothing to wait on is simply an `async` block that returns at once.
pub fn dependency_factory<F, Fut>(factory: F) -> DependencyFactory
where
    F: Fn(Arc<Dependencies>) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = DependencyFactoryResult> + Send + 'static,
{
    Arc::new(move |dependencies| factory(dependencies).boxed())
}

/// A resource the container can release when it closes.
///
/// Only consulted for a factory result whose binding owns it. Best effort, as the reference's is:
/// a failure to close is the resource's to swallow, and the container carries on closing the rest.
#[async_trait]
pub trait CloseDependency: Send + Sync {
    /// Releases the resource.
    async fn close(&self);
}

/// A resolved dependency: any shareable value, and how to close it if it can be closed.
#[derive(Clone)]
pub struct DependencyValue {
    value: Arc<dyn Any + Send + Sync>,
    closer: Option<Arc<dyn CloseDependency>>,
}

impl DependencyValue {
    /// A value with nothing to close.
    #[must_use]
    pub fn new<T: Any + Send + Sync>(value: Arc<T>) -> Self {
        Self {
            value,
            closer: None,
        }
    }

    /// A value the container closes when a binding that owns it closes.
    #[must_use]
    pub fn closable<T: CloseDependency + Any>(value: Arc<T>) -> Self {
        Self {
            closer: Some(value.clone()),
            value,
        }
    }

    /// A value read as one type and closed as another object: `closer` is what the container
    /// closes, and what it counts as the same object when deciding whether it closed it already.
    ///
    /// For a value that is read through a wrapper — a trait object packed so callers can find it by
    /// type — while the object behind the wrapper is the one that holds the resource.
    #[must_use]
    pub fn with_closer<T: Any + Send + Sync>(
        value: Arc<T>,
        closer: Arc<dyn CloseDependency>,
    ) -> Self {
        Self {
            value,
            closer: Some(closer),
        }
    }

    /// The value as `T`, or `None` when it is something else.
    #[must_use]
    pub fn downcast<T: Any + Send + Sync>(&self) -> Option<Arc<T>> {
        self.value.clone().downcast::<T>().ok()
    }

    /// Whether two resolutions returned the same object, not merely equal ones.
    #[must_use]
    pub fn same_as(&self, other: &Self) -> bool {
        self.identity() == other.identity()
    }

    /// The address of the shared value, which is what makes two resolutions the same object.
    fn identity(&self) -> usize {
        Arc::as_ptr(&self.value).cast::<()>() as usize
    }
}

impl fmt::Debug for DependencyValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DependencyValue")
            .field("closable", &self.closer.is_some())
            .finish_non_exhaustive()
    }
}

/// How a factory binding behaves: the reference's `cache`, `overwrite` and `owns_result` keyword
/// arguments, with the same defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FactoryOptions {
    cache: bool,
    overwrite: bool,
    owns_result: bool,
}

impl Default for FactoryOptions {
    fn default() -> Self {
        Self {
            cache: true,
            overwrite: false,
            owns_result: false,
        }
    }
}

impl FactoryOptions {
    /// Whether the factory runs once and its value is reused. `true` by default.
    #[must_use]
    pub const fn with_cache(mut self, cache: bool) -> Self {
        self.cache = cache;
        self
    }

    /// Whether a binding already under this key may be replaced. `false` by default.
    #[must_use]
    pub const fn with_overwrite(mut self, overwrite: bool) -> Self {
        self.overwrite = overwrite;
        self
    }

    /// Whether the container closes what the factory produced when it closes. `false` by default.
    #[must_use]
    pub const fn with_owns_result(mut self, owns_result: bool) -> Self {
        self.owns_result = owns_result;
        self
    }
}

/// Why a dependency could not be bound or resolved.
#[derive(Debug, Clone, thiserror::Error)]
#[non_exhaustive]
pub enum DependenciesError {
    /// A binding was attempted under the empty key.
    #[error("Dependency key must be non-empty")]
    EmptyKey,
    /// The key is bound already and the caller did not ask to replace it.
    #[error("Dependency `{key}` is already bound")]
    AlreadyBound {
        /// The key.
        key: DependencyKey,
    },
    /// The key was bound again while its factory was producing a value for the old binding.
    #[error("Dependency `{key}` was rebound while its factory was resolving")]
    Rebound {
        /// The key.
        key: DependencyKey,
    },
    /// Nothing is bound under the key a caller requires.
    #[error(
        "Missing dependency `{key}`{consumer_part}. Bind it on a Dependencies instance and pass it \
         as the dependencies of the sandbox client.",
        consumer_part = consumer.as_ref().map(|consumer| format!(" for {consumer}")).unwrap_or_default()
    )]
    Missing {
        /// The key.
        key: DependencyKey,
        /// Who needed it, when the caller said.
        consumer: Option<String>,
    },
    /// The container was closed before the resolution began.
    #[error("Dependencies container is closed; cannot resolve `{key}`")]
    Closed {
        /// The key.
        key: DependencyKey,
    },
    /// The container was closed while the key's factory was running.
    #[error("Dependencies container closed while resolving `{key}`")]
    ClosedWhileResolving {
        /// The key.
        key: DependencyKey,
    },
    /// The factory failed.
    #[error("{source}")]
    Factory {
        /// The key.
        key: DependencyKey,
        /// The factory's own failure, shared by everyone who waited on it.
        source: DependencyFactoryError,
    },
    /// The factory's task ended without producing anything — it panicked.
    #[error("the factory for dependency `{key}` did not finish")]
    FactoryAborted {
        /// The key.
        key: DependencyKey,
    },
    /// The value bound under the key is not the type the caller asked for.
    #[error("Dependency `{key}` is not a `{expected}`")]
    WrongType {
        /// The key.
        key: DependencyKey,
        /// The type the caller expected.
        expected: &'static str,
    },
}

impl DependenciesError {
    /// Whether this is a binding error, as opposed to a failure to resolve. The reference raises
    /// these as `DependenciesBindingError`, a `ValueError`.
    #[must_use]
    pub const fn is_binding_error(&self) -> bool {
        matches!(
            self,
            Self::EmptyKey | Self::AlreadyBound { .. } | Self::Rebound { .. }
        )
    }
}

/// One key's binding. Held behind an `Arc` so a factory can tell whether its binding is still the
/// one in force when it finishes.
enum Binding {
    Value(DependencyValue),
    Factory {
        factory: DependencyFactory,
        cache: bool,
        owns_result: bool,
    },
}

/// How a factory's task ended, as every waiter sees it.
type TaskOutcome = Result<Result<DependencyValue, DependenciesError>, TaskInterrupted>;

/// A factory's task, awaitable by any number of callers at once.
type SharedTask = Shared<BoxFuture<'static, TaskOutcome>>;

/// The task stopped without an answer: aborted by a rebind or a close, or panicked.
#[derive(Debug, Clone, Copy)]
enum TaskInterrupted {
    Aborted,
    Panicked,
}

/// A factory's task that is still running.
struct ActiveTask {
    abort: AbortHandle,
    outcome: SharedTask,
}

/// A cached factory's task that callers are waiting on.
struct PendingTask {
    id: u64,
    outcome: SharedTask,
}

#[derive(Default)]
struct State {
    bindings: HashMap<DependencyKey, Arc<Binding>>,
    cache: HashMap<DependencyKey, DependencyValue>,
    pending: HashMap<DependencyKey, PendingTask>,
    active: HashMap<u64, ActiveTask>,
    owned_results: Vec<DependencyValue>,
    next_task_id: u64,
    closed: bool,
    close_task: Option<Shared<BoxFuture<'static, ()>>>,
}

/// The session-scoped dependency container.
///
/// Shared as `Arc<Dependencies>`: a factory receives the container it was bound on, and resolution
/// runs factories as tasks that outlive any one caller.
#[derive(Default)]
pub struct Dependencies {
    state: Mutex<State>,
}

impl fmt::Debug for Dependencies {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.lock();
        let mut keys: Vec<&DependencyKey> = state.bindings.keys().collect();
        keys.sort();
        formatter
            .debug_struct("Dependencies")
            .field("keys", &keys)
            .field("closed", &state.closed)
            .finish_non_exhaustive()
    }
}

impl Dependencies {
    /// An empty container.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A container with each of `values` bound as a value.
    ///
    /// # Errors
    ///
    /// Returns [`DependenciesError::EmptyKey`] or [`DependenciesError::AlreadyBound`] for the first
    /// key that cannot be bound.
    pub fn with_values(
        values: impl IntoIterator<Item = (impl Into<DependencyKey>, DependencyValue)>,
    ) -> Result<Self, DependenciesError> {
        let dependencies = Self::new();
        for (key, value) in values {
            dependencies.bind_value(key, value, false)?;
        }
        Ok(dependencies)
    }

    /// Binds `value` under `key`.
    ///
    /// # Errors
    ///
    /// Returns [`DependenciesError::EmptyKey`] for the empty key, and
    /// [`DependenciesError::AlreadyBound`] when the key is bound and `overwrite` is not set.
    pub fn bind_value(
        &self,
        key: impl Into<DependencyKey>,
        value: DependencyValue,
        overwrite: bool,
    ) -> Result<&Self, DependenciesError> {
        self.bind(key.into(), Binding::Value(value), overwrite)?;
        Ok(self)
    }

    /// Binds `factory` under `key`.
    ///
    /// # Errors
    ///
    /// Returns [`DependenciesError::EmptyKey`] for the empty key, and
    /// [`DependenciesError::AlreadyBound`] when the key is bound and `options.overwrite` is not set.
    pub fn bind_factory(
        &self,
        key: impl Into<DependencyKey>,
        options: FactoryOptions,
        factory: DependencyFactory,
    ) -> Result<&Self, DependenciesError> {
        self.bind(
            key.into(),
            Binding::Factory {
                factory,
                cache: options.cache,
                owns_result: options.owns_result,
            },
            options.overwrite,
        )?;
        Ok(self)
    }

    /// A container with the same bindings and none of the state: no cached values, no factories
    /// running, nothing owned and not closed. This is the reference's `clone()`, which is how a
    /// client gives each session its own copy of the template it was configured with.
    #[must_use]
    pub fn clone_bindings(&self) -> Self {
        let state = self.lock();
        let bindings = state
            .bindings
            .iter()
            .map(|(key, binding)| {
                let copy = match binding.as_ref() {
                    Binding::Value(value) => Binding::Value(value.clone()),
                    Binding::Factory {
                        factory,
                        cache,
                        owns_result,
                    } => Binding::Factory {
                        factory: factory.clone(),
                        cache: *cache,
                        owns_result: *owns_result,
                    },
                };
                (key.clone(), Arc::new(copy))
            })
            .collect();
        Self {
            state: Mutex::new(State {
                bindings,
                ..State::default()
            }),
        }
    }

    /// The value bound under `key`, or `None` when nothing is.
    ///
    /// # Errors
    ///
    /// Returns the factory's failure, or the reason its value could not be handed out: the
    /// container closed, or the key was rebound, while it ran.
    pub async fn get(
        self: &Arc<Self>,
        key: &str,
    ) -> Result<Option<DependencyValue>, DependenciesError> {
        let binding = self.lock().bindings.get(key).cloned();
        match binding {
            None => Ok(None),
            Some(binding) => self.resolve(key, binding).await.map(Some),
        }
    }

    /// The value bound under `key`.
    ///
    /// # Errors
    ///
    /// Returns [`DependenciesError::Missing`] naming the key and `consumer` when nothing is bound,
    /// and otherwise whatever [`Self::get`] returns.
    pub async fn require(
        self: &Arc<Self>,
        key: &str,
        consumer: Option<&str>,
    ) -> Result<DependencyValue, DependenciesError> {
        self.get(key)
            .await?
            .ok_or_else(|| DependenciesError::Missing {
                key: key.to_owned(),
                consumer: consumer.map(str::to_owned),
            })
    }

    /// The value bound under `key`, as a `T`.
    ///
    /// # Errors
    ///
    /// Returns [`DependenciesError::WrongType`] when the value is something else, and otherwise
    /// whatever [`Self::require`] returns.
    pub async fn require_as<T: Any + Send + Sync>(
        self: &Arc<Self>,
        key: &str,
        consumer: Option<&str>,
    ) -> Result<Arc<T>, DependenciesError> {
        self.require(key, consumer)
            .await?
            .downcast::<T>()
            .ok_or_else(|| DependenciesError::WrongType {
                key: key.to_owned(),
                expected: std::any::type_name::<T>(),
            })
    }

    /// Closes the container: stops every factory still running, then closes every result a binding
    /// owns, newest first and each object once.
    ///
    /// Idempotent, and it keeps going without its caller: a caller that stops waiting leaves the
    /// close running, and the next call waits on that same close. Values bound directly are never
    /// closed.
    pub async fn close(self: &Arc<Self>) {
        let task = {
            let mut state = self.lock();
            if let Some(task) = &state.close_task {
                task.clone()
            } else {
                state.closed = true;
                let this = Arc::clone(self);
                let handle = tokio::spawn(async move { this.close_inner().await });
                let task = handle.map(|_| ()).boxed().shared();
                state.close_task = Some(task.clone());
                task
            }
        };
        task.await;
    }

    /// Whether [`Self::close`] has been called.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.lock().closed
    }

    fn bind(
        &self,
        key: DependencyKey,
        binding: Binding,
        overwrite: bool,
    ) -> Result<(), DependenciesError> {
        if key.is_empty() {
            return Err(DependenciesError::EmptyKey);
        }
        let stale_factory = {
            let mut state = self.lock();
            if !overwrite && state.bindings.contains_key(&key) {
                return Err(DependenciesError::AlreadyBound { key });
            }
            state.cache.remove(&key);
            // A factory still producing a value for the old binding is stopped: its value would
            // belong to a binding that no longer exists, and whoever was waiting on it is told the
            // key was rebound. It leaves `pending`, so the next resolution starts afresh, but stays
            // in `active` until it has actually stopped — see `TaskGuard`.
            let abort = state
                .pending
                .remove(&key)
                .and_then(|pending| state.active.get(&pending.id))
                .map(|active| active.abort.clone());
            state.bindings.insert(key, Arc::new(binding));
            abort
        };
        if let Some(abort) = stale_factory {
            abort.abort();
        }
        Ok(())
    }

    async fn resolve(
        self: &Arc<Self>,
        key: &str,
        binding: Arc<Binding>,
    ) -> Result<DependencyValue, DependenciesError> {
        let cache = match binding.as_ref() {
            // Returned even from a closed container: a value is not the container's to withdraw.
            Binding::Value(value) => return Ok(value.clone()),
            Binding::Factory { cache, .. } => *cache,
        };

        let (outcome, _abort_on_drop) = {
            let mut state = self.lock();
            if state.closed {
                return Err(DependenciesError::Closed {
                    key: key.to_owned(),
                });
            }
            if cache && let Some(value) = state.cache.get(key) {
                return Ok(value.clone());
            }
            if cache {
                let outcome = if let Some(pending) = state.pending.get(key) {
                    pending.outcome.clone()
                } else {
                    let (id, outcome) = self.spawn_factory(&mut state, key, &binding);
                    state.pending.insert(
                        key.to_owned(),
                        PendingTask {
                            id,
                            outcome: outcome.clone(),
                        },
                    );
                    outcome
                };
                // Shared by every waiter: this one giving up leaves the factory running for the
                // rest.
                (outcome, None)
            } else {
                let (id, outcome) = self.spawn_factory(&mut state, key, &binding);
                // Nobody else wants an uncached factory's value, so giving up stops it.
                (
                    outcome,
                    Some(AbortOnDrop {
                        dependencies: Arc::clone(self),
                        id,
                    }),
                )
            }
        };

        match outcome.await {
            Ok(result) => {
                let value = result?;
                self.check_binding_still_valid(key, &binding)?;
                Ok(value)
            }
            Err(TaskInterrupted::Aborted) => {
                self.check_binding_still_valid(key, &binding)?;
                // Aborted with the binding intact and the container open is not something this
                // container does, but a waiter must not be handed nothing as a value.
                Err(DependenciesError::FactoryAborted {
                    key: key.to_owned(),
                })
            }
            Err(TaskInterrupted::Panicked) => Err(DependenciesError::FactoryAborted {
                key: key.to_owned(),
            }),
        }
    }

    /// Starts `binding`'s factory as a task and registers it as running.
    fn spawn_factory(
        self: &Arc<Self>,
        state: &mut State,
        key: &str,
        binding: &Arc<Binding>,
    ) -> (u64, SharedTask) {
        let id = state.next_task_id;
        state.next_task_id += 1;
        let this = Arc::clone(self);
        let task_key = key.to_owned();
        let task_binding = Arc::clone(binding);
        let handle = tokio::spawn(async move {
            let _guard = TaskGuard {
                dependencies: Arc::clone(&this),
                key: task_key.clone(),
                id,
            };
            this.run_factory(task_key, task_binding).await
        });
        let abort = handle.abort_handle();
        let outcome = handle
            .map(|joined| match joined {
                Ok(result) => Ok(result),
                Err(error) if error.is_cancelled() => Err(TaskInterrupted::Aborted),
                Err(_) => Err(TaskInterrupted::Panicked),
            })
            .boxed()
            .shared();
        state.active.insert(
            id,
            ActiveTask {
                abort,
                outcome: outcome.clone(),
            },
        );
        (id, outcome)
    }

    /// The body of a factory's task.
    async fn run_factory(
        self: Arc<Self>,
        key: DependencyKey,
        binding: Arc<Binding>,
    ) -> Result<DependencyValue, DependenciesError> {
        let Binding::Factory {
            factory,
            cache,
            owns_result,
        } = binding.as_ref()
        else {
            unreachable!("only factory bindings are spawned");
        };
        let produced = factory(Arc::clone(&self)).await;

        let mut state = self.lock();
        let value = produced.map_err(|source| DependenciesError::Factory {
            key: key.clone(),
            source,
        })?;
        // Owned before the validity check, as the reference does it: a value produced for a binding
        // that has since been replaced, or by a container that has since closed, is still this
        // container's to close.
        if *owns_result {
            state.owned_results.push(value.clone());
        }
        Self::binding_still_valid(&state, &key, &binding)?;
        if *cache {
            state.cache.insert(key, value.clone());
        }
        Ok(value)
    }

    fn check_binding_still_valid(
        &self,
        key: &str,
        binding: &Arc<Binding>,
    ) -> Result<(), DependenciesError> {
        Self::binding_still_valid(&self.lock(), key, binding)
    }

    fn binding_still_valid(
        state: &State,
        key: &str,
        binding: &Arc<Binding>,
    ) -> Result<(), DependenciesError> {
        if state.closed {
            return Err(DependenciesError::ClosedWhileResolving {
                key: key.to_owned(),
            });
        }
        let current = state.bindings.get(key);
        if !current.is_some_and(|current| Arc::ptr_eq(current, binding)) {
            return Err(DependenciesError::Rebound {
                key: key.to_owned(),
            });
        }
        Ok(())
    }

    async fn close_inner(&self) {
        let running: Vec<(AbortHandle, SharedTask)> = {
            let state = self.lock();
            state
                .active
                .values()
                .map(|task| (task.abort.clone(), task.outcome.clone()))
                .collect()
        };
        for (abort, _) in &running {
            abort.abort();
        }
        // Waited for, not only aborted: a factory on another thread may be finishing right now,
        // and whatever it adds to the owned results has to be there before they are closed.
        futures::future::join_all(running.into_iter().map(|(_, outcome)| outcome)).await;

        let owned: Vec<DependencyValue> = self.lock().owned_results.clone();
        // Each object once, as the reference closes each `id(value)` once: the object is the one
        // the closer releases, however many values or wrappers were handed out for it.
        let mut already_closed: Vec<usize> = Vec::new();
        for value in owned.iter().rev() {
            let Some(closer) = &value.closer else {
                continue;
            };
            let identity = Arc::as_ptr(closer).cast::<()>() as usize;
            if already_closed.contains(&identity) {
                continue;
            }
            already_closed.push(identity);
            closer.close().await;
        }

        let mut state = self.lock();
        state.pending.clear();
        state.active.clear();
        state.cache.clear();
        state.owned_results.clear();
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // Nothing here panics while holding the lock, so a poisoned one still holds a consistent
        // state.
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Stops an uncached factory when the one caller waiting on it gives up.
struct AbortOnDrop {
    dependencies: Arc<Dependencies>,
    id: u64,
}

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        // After a normal finish the task has already removed itself, and this finds nothing. An
        // aborted task stays in `active` until it has stopped, so a close still waits for it.
        let abort = self
            .dependencies
            .lock()
            .active
            .get(&self.id)
            .map(|active| active.abort.clone());
        if let Some(abort) = abort {
            abort.abort();
        }
    }
}

/// Unregisters a factory's task when the task ends — returned, panicked or aborted alike.
///
/// Held inside the task, so it drops exactly when the task stops running. That is later than the
/// moment an abort is requested: a poll already under way on another thread finishes first, and may
/// still produce an owned value. Until then the task stays in `active`, so a close waits for it and
/// closes what it produced; and a cached factory that panicked is cleared from `pending`, so the
/// next resolution runs it again instead of rereading the failure.
struct TaskGuard {
    dependencies: Arc<Dependencies>,
    key: DependencyKey,
    id: u64,
}

impl Drop for TaskGuard {
    fn drop(&mut self) {
        let mut state = self.dependencies.lock();
        state.active.remove(&self.id);
        if state
            .pending
            .get(&self.key)
            .is_some_and(|pending| pending.id == self.id)
        {
            state.pending.remove(&self.key);
        }
    }
}
