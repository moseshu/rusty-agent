//! What every session holds for its caller besides its workspace: the dependency container, the
//! callbacks to run before it stops, the paths it created that no snapshot should keep, and the lock
//! that keeps two closes from interleaving.
//!
//! The reference keeps these as attributes of its base session class, so every backend has them
//! without writing any of it. A trait has no fields, so a backend embeds one of these and hands it
//! out from [`SandboxSession::resources`](super::session::SandboxSession::resources); the lifecycle
//! defaults do the rest.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, MutexGuard};

use futures::future::{BoxFuture, FutureExt};

use super::dependencies::Dependencies;
use super::error::SandboxError;
use super::session::SandboxResult;
use super::workspace_paths::PosixPath;

/// A callback run once before the session's workspace is persisted.
pub type PreStopHook = Arc<dyn Fn() -> BoxFuture<'static, SandboxResult<()>> + Send + Sync>;

/// Wraps an async closure as a [`PreStopHook`].
pub fn pre_stop_hook<F, Fut>(hook: F) -> PreStopHook
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = SandboxResult<()>> + Send + 'static,
{
    Arc::new(move || hook().boxed())
}

#[derive(Default)]
struct DependencySlot {
    current: Option<Arc<Dependencies>>,
    closed: bool,
}

#[derive(Default)]
struct HookState {
    /// `None` until the first registration, which is what tells "never registered" from "ran".
    hooks: Option<Vec<PreStopHook>>,
    ran: bool,
    failed: bool,
}

/// The per-session resources the lifecycle defaults use.
#[derive(Default)]
pub struct SessionResources {
    dependencies: Mutex<DependencySlot>,
    hooks: Mutex<HookState>,
    /// Workspace-relative paths this session created at runtime and no snapshot should keep.
    persist_skip_paths: Mutex<BTreeSet<PosixPath>>,
    /// Held across a whole run of the pre-stop callbacks, as the reference holds its lock.
    hooks_run: tokio::sync::Mutex<()>,
    /// Held across a whole close.
    close: tokio::sync::Mutex<()>,
}

impl std::fmt::Debug for SessionResources {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let hooks = lock(&self.hooks);
        formatter
            .debug_struct("SessionResources")
            .field("pre_stop_hooks", &hooks.hooks.as_ref().map_or(0, Vec::len))
            .field("pre_stop_hooks_failed", &hooks.failed)
            .finish_non_exhaustive()
    }
}

impl SessionResources {
    /// Resources with no dependencies bound and no callbacks registered.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The session's dependency container, created empty the first time it is asked for.
    #[must_use]
    pub fn dependencies(&self) -> Arc<Dependencies> {
        let mut slot = lock(&self.dependencies);
        if let Some(current) = &slot.current {
            return Arc::clone(current);
        }
        let created = Arc::new(Dependencies::new());
        slot.current = Some(Arc::clone(&created));
        slot.closed = false;
        created
    }

    /// Replaces the session's dependency container. `None` leaves the current one in place, as the
    /// reference does, so a client with no template does not wipe what a caller already set.
    pub fn set_dependencies(&self, dependencies: Option<Arc<Dependencies>>) {
        let Some(dependencies) = dependencies else {
            return;
        };
        let mut slot = lock(&self.dependencies);
        slot.current = Some(dependencies);
        slot.closed = false;
    }

    /// Registers a callback to run once before the workspace is persisted.
    ///
    /// Registering after the callbacks have run arms them again, and the next run runs every one of
    /// them — the new callback and the ones that already ran — which is what the reference does.
    pub fn register_pre_stop_hook(&self, hook: PreStopHook) {
        let mut state = lock(&self.hooks);
        state.hooks.get_or_insert_with(Vec::new).push(hook);
        state.ran = false;
    }

    /// Runs the registered callbacks, once.
    ///
    /// Every callback runs even after one fails; the first failure is returned and remembered.
    ///
    /// **A run that is cancelled counts as a failure.** The callbacks are marked as run before the
    /// first one starts, so a run dropped half way would otherwise leave them "ran, and fine" — and
    /// the next close would skip them and persist the workspace they were there to protect. The
    /// reference catches cancellation with every other exception here and records it the same way.
    ///
    /// # Errors
    ///
    /// Returns the first callback failure, on the call that ran them.
    pub async fn run_pre_stop_hooks(&self) -> SandboxResult<()> {
        let _running = self.hooks_run.lock().await;
        let hooks = {
            let mut state = lock(&self.hooks);
            match &state.hooks {
                Some(hooks) if !state.ran => {
                    let hooks = hooks.clone();
                    state.ran = true;
                    hooks
                }
                _ => return Ok(()),
            }
        };

        let mut unfinished = FailIfUnfinished {
            hooks: &self.hooks,
            finished: false,
        };
        let mut first_error: Option<SandboxError> = None;
        for hook in hooks {
            if let Err(error) = hook().await {
                first_error.get_or_insert(error);
            }
        }
        unfinished.finished = true;
        match first_error {
            None => Ok(()),
            Some(error) => {
                lock(&self.hooks).failed = true;
                Err(error)
            }
        }
    }

    /// Whether the callbacks have ever failed. Sticky: see
    /// [`SandboxSession::pre_stop_hooks_failed`](super::session::SandboxSession::pre_stop_hooks_failed).
    #[must_use]
    pub fn pre_stop_hooks_failed(&self) -> bool {
        lock(&self.hooks).failed
    }

    /// Closes the dependency container, once. A session that never had one has nothing to close.
    pub async fn close_dependencies(&self) {
        let dependencies = {
            let mut slot = lock(&self.dependencies);
            match &slot.current {
                Some(current) if !slot.closed => {
                    let current = Arc::clone(current);
                    slot.closed = true;
                    current
                }
                _ => return,
            }
        };
        dependencies.close().await;
    }

    /// Records a workspace-relative path that later snapshots leave out.
    ///
    /// Takes the path as given: checking that it names somewhere inside the workspace, and nowhere a
    /// mount owns, needs the manifest, which is the session's to supply. See
    /// [`SandboxSession::register_persist_workspace_skip_path`](super::session::SandboxSession::register_persist_workspace_skip_path).
    pub fn add_persist_workspace_skip_path(&self, path: PosixPath) {
        lock(&self.persist_skip_paths).insert(path);
    }

    /// The paths recorded by [`Self::add_persist_workspace_skip_path`].
    #[must_use]
    pub fn persist_workspace_skip_paths(&self) -> BTreeSet<PosixPath> {
        lock(&self.persist_skip_paths).clone()
    }

    /// Takes the lock a close holds from start to finish, so two closes cannot interleave.
    pub async fn lock_close(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.close.lock().await
    }
}

/// Records a pre-stop run that was dropped before it finished as a failed one.
struct FailIfUnfinished<'a> {
    hooks: &'a Mutex<HookState>,
    finished: bool,
}

impl Drop for FailIfUnfinished<'_> {
    fn drop(&mut self) {
        if !self.finished {
            lock(self.hooks).failed = true;
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // Nothing panics while holding these, so a poisoned lock still guards a consistent value.
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
