//! Reading and writing sandbox memory files under a configured layout.
//!
//! A port of the reference's `sandbox/memory/storage.py`. Everything goes through the session, as
//! the session's own user: the layout's directories, the two files the read side expects to exist,
//! the per-rollout raw memories, the concatenated `raw_memories.md` consolidation reads, and
//! `phase_two_selection.json`, which records what the last successful consolidation saw so the
//! next one can be told what was added, kept and dropped.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::SystemTime;

use futures::future::try_join_all;
use ra_core::sandbox::{
    ErrorCode, ExecRequest, MemoryLayoutConfig, PosixPath, SandboxError, SandboxResult,
    SandboxSession, ShellInvocation,
};
use serde::Serialize;
use serde_json::Value;
use tokio::sync::Mutex;

use super::json::{dumps_indented, python_strip, utc_isoformat};

/// One raw memory as consolidation selects it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PhaseTwoSelectionItem {
    rollout_id: String,
    updated_at: String,
    rollout_path: String,
    rollout_summary_file: String,
    terminal_state: String,
}

impl PhaseTwoSelectionItem {
    /// One selected raw memory.
    #[must_use]
    pub fn new(
        rollout_id: impl Into<String>,
        updated_at: impl Into<String>,
        rollout_path: impl Into<String>,
        rollout_summary_file: impl Into<String>,
        terminal_state: impl Into<String>,
    ) -> Self {
        Self {
            rollout_id: rollout_id.into(),
            updated_at: updated_at.into(),
            rollout_path: rollout_path.into(),
            rollout_summary_file: rollout_summary_file.into(),
            terminal_state: terminal_state.into(),
        }
    }

    /// Reads an entry of `phase_two_selection.json`, or `None` when it names no rollout or no
    /// summary file.
    ///
    /// Each field is read as the reference's `str(payload.get(key) or "").strip()` reads it.
    #[must_use]
    pub fn from_json(payload: &serde_json::Map<String, Value>) -> Option<Self> {
        let field = |key: &str| python_str_or_empty(payload.get(key));
        let rollout_id = field("rollout_id");
        let rollout_summary_file = field("rollout_summary_file");
        if rollout_id.is_empty() || rollout_summary_file.is_empty() {
            return None;
        }
        Some(Self {
            rollout_id,
            updated_at: field("updated_at"),
            rollout_path: field("rollout_path"),
            rollout_summary_file,
            terminal_state: field("terminal_state"),
        })
    }

    /// The rollout the raw memory came from.
    #[must_use]
    pub fn rollout_id(&self) -> &str {
        &self.rollout_id
    }

    /// When the rollout was last updated, or empty when unknown.
    #[must_use]
    pub fn updated_at(&self) -> &str {
        &self.updated_at
    }

    /// Where the rollout's JSONL file is, relative to the workspace root.
    #[must_use]
    pub fn rollout_path(&self) -> &str {
        &self.rollout_path
    }

    /// The rollout's summary file, relative to the memories directory.
    #[must_use]
    pub fn rollout_summary_file(&self) -> &str {
        &self.rollout_summary_file
    }

    /// How the rollout ended.
    #[must_use]
    pub fn terminal_state(&self) -> &str {
        &self.terminal_state
    }
}

/// `str(value or "").strip()` for a JSON value.
fn python_str_or_empty(value: Option<&Value>) -> String {
    let text = match value {
        None | Some(Value::Null | Value::Bool(false)) => String::new(),
        Some(Value::Bool(true)) => "True".to_owned(),
        Some(Value::String(text)) => text.clone(),
        Some(Value::Number(number)) if number.as_f64() == Some(0.0) => String::new(),
        Some(Value::Number(number)) => number.to_string(),
        Some(Value::Array(items)) if items.is_empty() => String::new(),
        Some(Value::Object(fields)) if fields.is_empty() => String::new(),
        Some(other) => other.to_string(),
    };
    python_strip(&text).to_owned()
}

/// What one consolidation is given, and how it differs from the last successful one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PhaseTwoInputSelection {
    selected: Vec<PhaseTwoSelectionItem>,
    retained_rollout_ids: BTreeSet<String>,
    removed: Vec<PhaseTwoSelectionItem>,
}

impl PhaseTwoInputSelection {
    /// A selection: what is selected, which of it the last successful consolidation also had, and
    /// what it had that is no longer selected.
    #[must_use]
    pub const fn new(
        selected: Vec<PhaseTwoSelectionItem>,
        retained_rollout_ids: BTreeSet<String>,
        removed: Vec<PhaseTwoSelectionItem>,
    ) -> Self {
        Self {
            selected,
            retained_rollout_ids,
            removed,
        }
    }

    /// The raw memories consolidation is given, most recent first.
    #[must_use]
    pub fn selected(&self) -> &[PhaseTwoSelectionItem] {
        &self.selected
    }

    /// The selected rollouts the last successful consolidation also had.
    #[must_use]
    pub const fn retained_rollout_ids(&self) -> &BTreeSet<String> {
        &self.retained_rollout_ids
    }

    /// What the last successful consolidation had that is no longer selected.
    #[must_use]
    pub fn removed(&self) -> &[PhaseTwoSelectionItem] {
        &self.removed
    }
}

/// The contents of `phase_two_selection.json`, in the reference's key order.
#[derive(Serialize)]
struct SelectionFile<'a> {
    version: u32,
    updated_at: String,
    selected: &'a [PhaseTwoSelectionItem],
}

/// Reads and writes sandbox memory files through a session.
pub struct SandboxMemoryStorage {
    session: Arc<dyn SandboxSession>,
    layout: MemoryLayoutConfig,
    layout_lock: Mutex<()>,
}

impl std::fmt::Debug for SandboxMemoryStorage {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SandboxMemoryStorage")
            .field("backend", &self.session.backend_id())
            .field("layout", &self.layout)
            .finish_non_exhaustive()
    }
}

impl SandboxMemoryStorage {
    /// Memory files of `session`, kept where `layout` says.
    #[must_use]
    pub fn new(session: Arc<dyn SandboxSession>, layout: MemoryLayoutConfig) -> Self {
        Self {
            session,
            layout,
            layout_lock: Mutex::new(()),
        }
    }

    /// The per-rollout JSONL directory, relative to the workspace root.
    #[must_use]
    pub fn sessions_dir(&self) -> PosixPath {
        self.layout.sessions_path()
    }

    /// The memories directory, relative to the workspace root.
    #[must_use]
    pub fn memories_dir(&self) -> PosixPath {
        self.layout.memories_path()
    }

    /// Where per-rollout raw memories are kept.
    #[must_use]
    pub fn raw_memories_dir(&self) -> PosixPath {
        self.memories_dir().join("raw_memories")
    }

    /// Where per-rollout summaries are kept.
    #[must_use]
    pub fn rollout_summaries_dir(&self) -> PosixPath {
        self.memories_dir().join("rollout_summaries")
    }

    /// Where the last successful consolidation's selection is recorded.
    #[must_use]
    pub fn phase_two_selection_path(&self) -> PosixPath {
        self.memories_dir().join("phase_two_selection.json")
    }

    /// Creates the layout's directories, and an empty `MEMORY.md` and `memory_summary.md` where
    /// there is none.
    ///
    /// # Errors
    ///
    /// Returns the session's failure.
    pub async fn ensure_layout(&self) -> SandboxResult<()> {
        let _guard = self.layout_lock.lock().await;
        let memories_dir = self.memories_dir();
        let directories = [
            self.sessions_dir(),
            memories_dir.clone(),
            memories_dir.join("raw_memories"),
            memories_dir.join("rollout_summaries"),
            memories_dir.join("skills"),
        ];
        try_join_all(
            directories
                .iter()
                .map(|directory| self.session.mkdir(directory.as_str(), true, None)),
        )
        .await?;
        self.ensure_text_file(&memories_dir.join("MEMORY.md"))
            .await?;
        self.ensure_text_file(&memories_dir.join("memory_summary.md"))
            .await
    }

    /// Writes an empty file at `path` unless a regular file is already there.
    ///
    /// # Errors
    ///
    /// Returns the session's failure.
    pub async fn ensure_text_file(&self, path: &PosixPath) -> SandboxResult<()> {
        let absolute = self
            .session
            .workspace_path_policy()?
            .normalize_sandbox_path(path.as_str(), false)?;
        let exists = self
            .session
            .exec(
                ExecRequest::new(["test".to_owned(), "-f".to_owned(), absolute.to_string()])
                    .with_shell(ShellInvocation::None),
            )
            .await?;
        if exists.ok() {
            return Ok(());
        }
        self.session.write(path.as_str(), Vec::new(), None).await
    }

    /// Reads a file as text, replacing what is not UTF-8.
    ///
    /// # Errors
    ///
    /// Returns the session's failure, [`ErrorCode::WorkspaceReadNotFound`] for a missing file.
    pub async fn read_text(&self, path: &PosixPath) -> SandboxResult<String> {
        let payload = self.session.read(path.as_str(), None).await?;
        Ok(String::from_utf8_lossy(&payload).into_owned())
    }

    /// Writes `text` to a file.
    ///
    /// # Errors
    ///
    /// Returns the session's failure.
    pub async fn write_text(&self, path: &PosixPath, text: &str) -> SandboxResult<()> {
        self.session
            .write(path.as_str(), text.as_bytes().to_vec(), None)
            .await
    }

    /// The most recent `max_raw_memories_for_consolidation` raw memories, compared with the last
    /// successful consolidation's selection.
    ///
    /// # Errors
    ///
    /// Returns the session's failure to read a raw memory or the recorded selection, other than
    /// its absence.
    pub async fn build_phase_two_input_selection(
        &self,
        max_raw_memories_for_consolidation: usize,
    ) -> SandboxResult<PhaseTwoInputSelection> {
        let mut selected = self.list_current_selection_items().await?;
        selected.truncate(max_raw_memories_for_consolidation);
        let prior_selected = self.read_phase_two_selection().await?;
        let selected_ids: BTreeSet<String> = selected
            .iter()
            .map(|item| item.rollout_id.clone())
            .collect();
        let prior_ids: BTreeSet<String> = prior_selected
            .iter()
            .map(|item| item.rollout_id.clone())
            .collect();
        Ok(PhaseTwoInputSelection {
            retained_rollout_ids: selected_ids.intersection(&prior_ids).cloned().collect(),
            removed: prior_selected
                .into_iter()
                .filter(|item| !selected_ids.contains(&item.rollout_id))
                .collect(),
            selected,
        })
    }

    /// Writes the selected raw memories, joined, to `raw_memories.md`; answers whether there was
    /// any to write.
    ///
    /// # Errors
    ///
    /// Returns the session's failure, other than a selected raw memory that is missing.
    pub async fn rebuild_raw_memories(
        &self,
        selected_items: &[PhaseTwoSelectionItem],
    ) -> SandboxResult<bool> {
        let mut chunks = Vec::new();
        for item in selected_items {
            let path = self
                .raw_memories_dir()
                .join(&format!("{}.md", item.rollout_id));
            match self.read_text(&path).await {
                Ok(text) => chunks.push(text.trim_end_matches('\n').to_owned()),
                Err(error) if error.error_code() == ErrorCode::WorkspaceReadNotFound => {}
                Err(error) => return Err(error),
            }
        }
        if chunks.is_empty() {
            return Ok(false);
        }
        self.write_text(
            &self.memories_dir().join("raw_memories.md"),
            &chunks.join("\n\n"),
        )
        .await?;
        Ok(true)
    }

    /// The last successful consolidation's selection; empty when it was never recorded or cannot
    /// be read as one.
    ///
    /// # Errors
    ///
    /// Returns the session's failure to read the file, other than its absence.
    pub async fn read_phase_two_selection(&self) -> SandboxResult<Vec<PhaseTwoSelectionItem>> {
        let raw = match self.read_text(&self.phase_two_selection_path()).await {
            Ok(raw) => raw,
            Err(error) if error.error_code() == ErrorCode::WorkspaceReadNotFound => {
                return Ok(Vec::new());
            }
            Err(error) => return Err(error),
        };
        let Ok(Value::Object(payload)) = serde_json::from_str::<Value>(&raw) else {
            return Ok(Vec::new());
        };
        let Some(Value::Array(selected)) = payload.get("selected") else {
            return Ok(Vec::new());
        };
        Ok(selected
            .iter()
            .filter_map(Value::as_object)
            .filter_map(PhaseTwoSelectionItem::from_json)
            .collect())
    }

    /// Records `selected_items` as the last successful consolidation's selection.
    ///
    /// # Errors
    ///
    /// Returns the session's failure.
    pub async fn write_phase_two_selection(
        &self,
        selected_items: &[PhaseTwoSelectionItem],
    ) -> SandboxResult<()> {
        let payload = SelectionFile {
            version: 1,
            updated_at: utc_isoformat(SystemTime::now()),
            selected: selected_items,
        };
        self.write_text(
            &self.phase_two_selection_path(),
            &format!(
                "{}\n",
                dumps_indented(&payload).map_err(|error| {
                    SandboxError::new(
                        ErrorCode::SandboxConfigInvalid,
                        ra_core::sandbox::OpName::Write,
                        "failed to serialize memory selection",
                    )
                    .with_cause(error)
                })?
            ),
        )
        .await
    }

    /// Every raw memory that names its rollout and summary, most recent first; a directory that
    /// cannot be listed has none.
    async fn list_current_selection_items(&self) -> SandboxResult<Vec<PhaseTwoSelectionItem>> {
        let raw_memories_dir = self.raw_memories_dir();
        let Ok(entries) = self.session.ls(raw_memories_dir.as_str(), None).await else {
            return Ok(Vec::new());
        };

        let mut items = Vec::new();
        for entry in entries {
            if entry.is_dir() {
                continue;
            }
            let entry_path = PosixPath::new(&entry.path);
            let Some(name) = entry_path.parts().last().map(|part| (*part).to_owned()) else {
                continue;
            };
            if python_suffix(&name) != ".md" {
                continue;
            }
            let raw_memory = match self.read_text(&raw_memories_dir.join(&name)).await {
                Ok(text) => text.trim_end_matches('\n').to_owned(),
                Err(error) if error.error_code() == ErrorCode::WorkspaceReadNotFound => continue,
                Err(error) => return Err(error),
            };
            if let Some(item) = extract_selection_item(&raw_memory) {
                items.push((updated_at_sort_key(&raw_memory), item));
            }
        }
        // Most recent first. The comparison is reversed rather than the sorted list, so equal keys
        // keep the order they were listed in, as the reference's stable `reverse=True` sort does.
        items.sort_by(|(left_key, left), (right_key, right)| {
            (right_key, &right.rollout_id).cmp(&(left_key, &left.rollout_id))
        });
        Ok(items.into_iter().map(|(_, item)| item).collect())
    }
}

/// The last extension of a file name, as a path's `suffix` answers: empty for a name without a
/// dot, one that only starts with a dot, or one that ends with one.
fn python_suffix(name: &str) -> &str {
    match name.rfind('.') {
        Some(index) if index > 0 && index < name.len() - 1 => &name[index..],
        _ => "",
    }
}

/// How recent a raw memory is: `(1, timestamp)` for one with a known `updated_at`, and `(0, "")`
/// otherwise, so unknown timestamps sort last when the most recent come first.
#[must_use]
pub fn updated_at_sort_key(raw_memory: &str) -> (u8, String) {
    for line in raw_memory.lines() {
        if let Some(value) = line.strip_prefix("updated_at:") {
            let updated_at = python_strip(value);
            if updated_at.is_empty() || updated_at == "unknown" {
                return (0, String::new());
            }
            return (1, updated_at.to_owned());
        }
    }
    (0, String::new())
}

/// The selection entry a raw memory's header describes, or `None` when it names no rollout or no
/// summary file.
fn extract_selection_item(raw_memory: &str) -> Option<PhaseTwoSelectionItem> {
    let rollout_id = extract_metadata_value(raw_memory, "rollout_id");
    let rollout_summary_file = extract_metadata_value(raw_memory, "rollout_summary_file");
    if rollout_id.is_empty() || rollout_summary_file.is_empty() {
        return None;
    }
    Some(PhaseTwoSelectionItem {
        rollout_id,
        updated_at: extract_metadata_value(raw_memory, "updated_at"),
        rollout_path: extract_metadata_value(raw_memory, "rollout_path"),
        rollout_summary_file,
        terminal_state: extract_metadata_value(raw_memory, "terminal_state"),
    })
}

/// The first `key:` header line's value, trimmed, or empty.
fn extract_metadata_value(raw_memory: &str, key: &str) -> String {
    let prefix = format!("{key}:");
    raw_memory
        .lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .map(|value| python_strip(value).to_owned())
        .unwrap_or_default()
}
