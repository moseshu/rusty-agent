//! The generation manager: appending run segments during a sandbox session, and extracting and
//! consolidating them when the session closes.
//!
//! A port of the reference's `sandbox/memory/manager.py`. One manager serves one memory layout in
//! one session, and the runs of that session share it: each run appends its segment to its
//! rollout's file, and the manager's flush, registered as the session's pre-stop callback, runs
//! phase one for every rollout it saw and then one phase two.
//!
//! # Deviations from the reference
//!
//! - **The session is held weakly.** The reference's manager holds its session, and the session
//!   holds the manager through the callback it registered; its garbage collector breaks that
//!   cycle. Here the manager holds the session weakly, as the reference's registry does, so a
//!   session nobody else holds is released with its callbacks. A manager whose session is gone has
//!   nothing left to do.
//! - **Rollouts are extracted in the flush itself.** The reference starts its worker task when the
//!   flush begins, queues every rollout and a stop marker, and waits for the worker. That is one
//!   rollout after another, in sorted order, with each failure logged; here the flush does exactly
//!   that directly. A flush that is dropped part way unregisters the manager and runs no
//!   consolidation, as a cancelled one does on the reference.
//! - **Model runs need a resolver.** The reference resolves phase models through its global default
//!   provider; here they are resolved with the resolver of the run that created the manager.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::{Arc, LazyLock, Mutex as StdMutex, MutexGuard, PoisonError, Weak};

use futures::future::try_join;
use ra_core::{
    error::{Error, Result},
    item::ModelInputItem,
    model::ModelResolver,
    sandbox::{
        MemoryGenerateConfig, PosixPath, SandboxMemory, SandboxResult, SandboxSession,
        pre_stop_hook,
    },
};
use serde_json::Value;
use tokio::sync::Mutex;
use tracing::warn;

use super::agents::MemoryRunConfig;
use super::json::{python_repr, python_rstrip, python_truthy_str};
use super::phase_one::{
    normalize_rollout_slug, parse_rollout_segments, render_phase_one_prompt,
    rollout_id_from_rollout_path, run_phase_one, validate_rollout_artifacts,
};
use super::phase_two::run_phase_two;
use super::rollouts::{
    RolloutPayload, build_rollout_payload_from_result, rollout_file_name_for_rollout_id,
    validate_rollout_id, write_rollout,
};
use super::storage::SandboxMemoryStorage;
use crate::runner::RunResult;
use crate::sandbox::sandbox_error;

/// The memories and sessions directories a manager serves, normalized.
type LayoutKey = (String, String);

/// The managers attached to one session, by layout.
struct SessionManagers {
    session: Weak<dyn SandboxSession>,
    managers: BTreeMap<LayoutKey, Arc<SandboxMemoryGenerationManager>>,
}

/// The reference's `_MEMORY_GENERATION_MANAGERS`: sessions are held weakly, and an entry whose
/// session is gone is dropped the next time the registry is used.
static MANAGERS: LazyLock<StdMutex<Vec<SessionManagers>>> =
    LazyLock::new(|| StdMutex::new(Vec::new()));

fn registry() -> MutexGuard<'static, Vec<SessionManagers>> {
    // Nothing panics while holding it, so a poisoned lock still guards a consistent registry.
    let mut registry = MANAGERS.lock().unwrap_or_else(PoisonError::into_inner);
    registry.retain(|entry| entry.session.strong_count() > 0);
    registry
}

/// The address a session lives at, which identifies it however it is held.
fn session_address(session: &Arc<dyn SandboxSession>) -> *const () {
    Arc::as_ptr(session).cast::<()>()
}

fn is_session(entry: &SessionManagers, address: *const ()) -> bool {
    entry.session.as_ptr().cast::<()>() == address
}

fn layout_key(memory: &SandboxMemory) -> LayoutKey {
    let layout = memory.layout();
    (
        layout.memories_path().normalized().as_str().to_owned(),
        layout.sessions_path().normalized().as_str().to_owned(),
    )
}

/// The manager for `memory`'s layout in `session`, creating and registering one if there is none.
///
/// The reference's `get_or_create_memory_generation_manager`. A session can host several
/// generating memory capabilities when their layouts differ; capabilities that share a layout
/// share its manager, and must then generate the same way. `resolver` resolves the phase models of
/// a manager created here.
///
/// # Errors
///
/// Returns a configuration error, worded as the reference's, when the layout already has a manager
/// generating differently, when another manager uses the same memories directory with a different
/// sessions directory or the same sessions directory with a different memories directory, and when
/// `memory` does not generate.
pub fn get_or_create_memory_generation_manager(
    session: &Arc<dyn SandboxSession>,
    memory: &SandboxMemory,
    resolver: &Arc<dyn ModelResolver>,
) -> Result<Arc<SandboxMemoryGenerationManager>> {
    let key = layout_key(memory);
    let address = session_address(session);
    let mut registry = registry();
    if let Some(entry) = registry.iter().find(|entry| is_session(entry, address)) {
        if let Some(existing) = entry.managers.get(&key) {
            if existing.memory.generate() != memory.generate() {
                return Err(Error::config(
                    "Sandbox session already has a different Memory generation config attached \
                     for this memory layout.",
                ));
            }
            return Ok(Arc::clone(existing));
        }
        let (memories_dir, sessions_dir) = &key;
        for (existing_memories_dir, existing_sessions_dir) in entry.managers.keys() {
            if existing_memories_dir == memories_dir {
                return Err(Error::config(format!(
                    "Sandbox session already has a Memory generation capability for \
                     memories_dir={}. Use a different memories_dir for isolated memories, or the \
                     same layout to share memory.",
                    python_repr(memories_dir)
                )));
            }
            if existing_sessions_dir == sessions_dir {
                return Err(Error::config(format!(
                    "Sandbox session already has a Memory generation capability for \
                     sessions_dir={}. Use a different sessions_dir for isolated memories, or the \
                     same layout to share memory.",
                    python_repr(sessions_dir)
                )));
            }
        }
    }

    let manager = SandboxMemoryGenerationManager::new(session, memory.clone(), resolver)?;
    match registry.iter_mut().find(|entry| is_session(entry, address)) {
        Some(entry) => {
            entry.managers.insert(key, Arc::clone(&manager));
        }
        None => registry.push(SessionManagers {
            session: Arc::downgrade(session),
            managers: BTreeMap::from([(key, Arc::clone(&manager))]),
        }),
    }
    Ok(manager)
}

/// The managers registered for `session`, in layout order.
///
/// Empty once every one of them has flushed.
#[must_use]
pub fn memory_generation_managers(
    session: &Arc<dyn SandboxSession>,
) -> Vec<Arc<SandboxMemoryGenerationManager>> {
    let address = session_address(session);
    registry()
        .iter()
        .find(|entry| is_session(entry, address))
        .map(|entry| entry.managers.values().cloned().collect())
        .unwrap_or_default()
}

/// Removes `manager` from its session's entry, and the entry once it is empty.
fn unregister(manager: &SandboxMemoryGenerationManager) {
    let address = manager.session.as_ptr().cast::<()>();
    let key = layout_key(&manager.memory);
    let mut registry = registry();
    if let Some(entry) = registry.iter_mut().find(|entry| is_session(entry, address))
        && entry
            .managers
            .get(&key)
            .is_some_and(|existing| std::ptr::eq(Arc::as_ptr(existing), manager))
    {
        entry.managers.remove(&key);
    }
    registry.retain(|entry| !(is_session(entry, address) && entry.managers.is_empty()));
}

/// Unregisters the manager however its flush ends, a flush dropped part way included.
struct UnregisterOnDrop<'a>(&'a SandboxMemoryGenerationManager);

impl Drop for UnregisterOnDrop<'_> {
    fn drop(&mut self) {
        unregister(self.0);
    }
}

/// What the manager has seen, guarded by the lock its appends and its flush share.
#[derive(Default)]
struct ManagerState {
    rollout_files_by_rollout_id: BTreeMap<String, String>,
    pending_phase_two_rollout_ids: Vec<String>,
    stopped: bool,
}

/// Background memory generation for one memory layout in one sandbox session.
///
/// The reference's `SandboxMemoryGenerationManager`.
pub struct SandboxMemoryGenerationManager {
    session: Weak<dyn SandboxSession>,
    memory: SandboxMemory,
    generate: MemoryGenerateConfig,
    resolver: Arc<dyn ModelResolver>,
    state: Mutex<ManagerState>,
}

impl SandboxMemoryGenerationManager {
    /// A manager for `memory` in `session`, with its flush registered as the session's pre-stop
    /// callback.
    fn new(
        session: &Arc<dyn SandboxSession>,
        memory: SandboxMemory,
        resolver: &Arc<dyn ModelResolver>,
    ) -> Result<Arc<Self>> {
        let Some(generate) = memory.generate().cloned() else {
            return Err(Error::config(
                "SandboxMemoryGenerationManager requires `Memory.generate` to be set.",
            ));
        };
        let manager = Arc::new(Self {
            session: Arc::downgrade(session),
            memory,
            generate,
            resolver: Arc::clone(resolver),
            state: Mutex::new(ManagerState::default()),
        });
        let flushing = Arc::clone(&manager);
        session.register_pre_stop_hook(pre_stop_hook(move || {
            let manager = Arc::clone(&flushing);
            async move { manager.flush().await }
        }));
        Ok(manager)
    }

    /// The memory this manager generates.
    #[must_use]
    pub const fn memory(&self) -> &SandboxMemory {
        &self.memory
    }

    /// Records a finished run as a segment of rollout `rollout_id`, with `input_override` as its
    /// input when given.
    ///
    /// The reference's `enqueue_result`.
    ///
    /// # Errors
    ///
    /// As [`Self::enqueue_rollout_payload`].
    pub async fn enqueue_result(
        &self,
        result: &RunResult,
        input_override: Option<&[ModelInputItem]>,
        rollout_id: &str,
    ) -> Result<()> {
        self.enqueue_rollout_payload(
            build_rollout_payload_from_result(result, input_override),
            rollout_id,
        )
        .await
    }

    /// Appends `payload` to rollout `rollout_id`'s file under the sessions directory, recorded
    /// under that id, for extraction when the session closes. Nothing is appended once the manager
    /// has flushed.
    ///
    /// The reference's `enqueue_rollout_payload`.
    ///
    /// # Errors
    ///
    /// Returns a configuration error, worded as the reference's, for an id that is not file-safe,
    /// and the session's failure to write.
    pub async fn enqueue_rollout_payload(
        &self,
        payload: RolloutPayload,
        rollout_id: &str,
    ) -> Result<()> {
        let mut state = self.state.lock().await;
        if state.stopped {
            return Ok(());
        }
        let Some(session) = self.session.upgrade() else {
            return Ok(());
        };
        self.storage(&session)
            .ensure_layout()
            .await
            .map_err(sandbox_error)?;
        let rollout_id = validate_rollout_id(rollout_id)?;
        let file_name = rollout_file_name_for_rollout_id(&rollout_id)?;
        let rollout_file = write_rollout(
            &session,
            &payload.with_rollout_id(rollout_id.clone()),
            self.memory.layout().sessions_dir(),
            Some(&file_name),
        )
        .await?;
        let name = rollout_file
            .parts()
            .last()
            .map_or(file_name, |name| (*name).to_owned());
        state.rollout_files_by_rollout_id.insert(rollout_id, name);
        Ok(())
    }

    /// Extracts every rollout this manager recorded, then consolidates once, and unregisters the
    /// manager. Runs once; later calls do nothing.
    ///
    /// The reference's `flush`. A rollout whose extraction fails is logged and skipped, and a
    /// failed consolidation is logged and leaves the selection file as it was.
    ///
    /// # Errors
    ///
    /// Returns the session's failure to lay out, read or write the memory files outside of those
    /// two steps.
    pub async fn flush(&self) -> SandboxResult<()> {
        let mut state = self.state.lock().await;
        if state.stopped {
            return Ok(());
        }
        state.stopped = true;
        let _unregister = UnregisterOnDrop(self);

        let rollout_files: BTreeSet<String> = state
            .rollout_files_by_rollout_id
            .values()
            .cloned()
            .collect();
        if rollout_files.is_empty() {
            return Ok(());
        }
        let Some(session) = self.session.upgrade() else {
            return Ok(());
        };
        let storage = self.storage(&session);
        storage.ensure_layout().await?;
        let run = MemoryRunConfig::new(
            Arc::clone(&session),
            Arc::clone(&self.resolver),
            self.memory.phase_capabilities().to_vec(),
        );
        for rollout_file in &rollout_files {
            match self
                .process_rollout_file(&storage, &run, &session, rollout_file)
                .await
            {
                Ok(Some(rollout_id)) => state.pending_phase_two_rollout_ids.push(rollout_id),
                Ok(None) => {}
                Err(error) => warn!(error = %error, "Sandbox memory worker failed"),
            }
        }
        self.run_phase_two(&storage, &run, &mut state).await
    }

    fn storage(&self, session: &Arc<dyn SandboxSession>) -> SandboxMemoryStorage {
        SandboxMemoryStorage::new(Arc::clone(session), self.memory.layout().clone())
    }

    /// Extracts one rollout into its raw memory and rollout summary, answering its id, or `None`
    /// when there was nothing worth remembering.
    async fn process_rollout_file(
        &self,
        storage: &SandboxMemoryStorage,
        run: &MemoryRunConfig,
        session: &Arc<dyn SandboxSession>,
        rollout_file_name: &str,
    ) -> Result<Option<String>> {
        let sessions_dir = storage.sessions_dir();
        let rollout_contents = storage
            .read_text(&sessions_dir.join(rollout_file_name))
            .await
            .map_err(sandbox_error)?;

        let phase_one_prompt = render_phase_one_prompt(&rollout_contents)?;
        let artifacts = run_phase_one(&self.generate, phase_one_prompt, run).await?;
        if !validate_rollout_artifacts(&artifacts)? {
            return Ok(None);
        }

        let segments = parse_rollout_segments(&rollout_contents)?;
        let Some(payload) = segments.last() else {
            return Ok(None);
        };
        let updated_at = payload
            .get("updated_at")
            .and_then(python_truthy_str)
            .unwrap_or_else(|| "unknown".to_owned());
        let terminal_state = match payload.get("terminal_metadata") {
            Some(Value::Object(metadata)) => metadata
                .get("terminal_state")
                .and_then(python_truthy_str)
                .unwrap_or_else(|| "unknown".to_owned()),
            _ => "unknown".to_owned(),
        };

        let rollout_id = rollout_id_from_rollout_path(rollout_file_name)?;
        let rollout_slug = normalize_rollout_slug(artifacts.rollout_slug())?;
        let rollout_path = sessions_dir.join(rollout_file_name);
        let rollout_summary_file = format!("rollout_summaries/{rollout_id}_{rollout_slug}.md");
        let memories_dir = storage.memories_dir();
        try_join(
            storage.write_text(
                &memories_dir
                    .join("raw_memories")
                    .join(&format!("{rollout_id}.md")),
                &format_raw_memory(&RawMemoryHeader {
                    updated_at: &updated_at,
                    rollout_id: &rollout_id,
                    rollout_path: &rollout_path,
                    rollout_summary_file: &rollout_summary_file,
                    terminal_state: &terminal_state,
                    raw_memory: artifacts.raw_memory(),
                }),
            ),
            storage.write_text(
                &memories_dir.join(&rollout_summary_file),
                &format_rollout_summary(
                    &updated_at,
                    &rollout_path,
                    &session.state().session_id().to_string(),
                    &terminal_state,
                    artifacts.rollout_summary(),
                ),
            ),
        )
        .await
        .map_err(sandbox_error)?;
        Ok(Some(rollout_id))
    }

    /// Consolidates the most recent raw memories, if any rollout was extracted, and records what
    /// the consolidation saw once it succeeds.
    async fn run_phase_two(
        &self,
        storage: &SandboxMemoryStorage,
        run: &MemoryRunConfig,
        state: &mut ManagerState,
    ) -> SandboxResult<()> {
        if state.pending_phase_two_rollout_ids.is_empty() {
            return Ok(());
        }
        let rollout_ids: BTreeSet<String> = state
            .pending_phase_two_rollout_ids
            .iter()
            .cloned()
            .collect();
        let selection = storage
            .build_phase_two_input_selection(
                usize::try_from(self.generate.max_raw_memories_for_consolidation())
                    .unwrap_or(usize::MAX),
            )
            .await?;
        if !storage.rebuild_raw_memories(selection.selected()).await? {
            return Ok(());
        }
        if let Err(error) = run_phase_two(
            &self.generate,
            self.memory.layout().memories_dir(),
            &selection,
            run,
        )
        .await
        {
            warn!(error = %error, "Sandbox memory phase 2 failed");
            return Ok(());
        }
        storage
            .write_phase_two_selection(selection.selected())
            .await?;
        state
            .pending_phase_two_rollout_ids
            .retain(|rollout_id| !rollout_ids.contains(rollout_id));
        Ok(())
    }
}

impl fmt::Debug for SandboxMemoryGenerationManager {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SandboxMemoryGenerationManager")
            .field("memory", &self.memory)
            .field("session_alive", &(self.session.strong_count() > 0))
            .finish_non_exhaustive()
    }
}

/// The header lines of a raw memory file.
struct RawMemoryHeader<'a> {
    updated_at: &'a str,
    rollout_id: &'a str,
    rollout_path: &'a PosixPath,
    rollout_summary_file: &'a str,
    terminal_state: &'a str,
    raw_memory: &'a str,
}

/// The reference's `_format_raw_memory`.
fn format_raw_memory(header: &RawMemoryHeader<'_>) -> String {
    format!(
        "rollout_id: {}\nupdated_at: {}\nrollout_path: {}\nrollout_summary_file: {}\n\
         terminal_state: {}\n\n{}\n",
        header.rollout_id,
        header.updated_at,
        header.rollout_path,
        header.rollout_summary_file,
        header.terminal_state,
        python_rstrip(header.raw_memory),
    )
}

/// The reference's `_format_rollout_summary`.
fn format_rollout_summary(
    updated_at: &str,
    rollout_path: &PosixPath,
    session_id: &str,
    terminal_state: &str,
    rollout_summary: &str,
) -> String {
    format!(
        "session_id: {session_id}\nupdated_at: {updated_at}\nrollout_path: {rollout_path}\n\
         terminal_state: {terminal_state}\n\n{}\n",
        python_rstrip(rollout_summary),
    )
}
