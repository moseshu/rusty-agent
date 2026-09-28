//! What sandbox memory is configured with: where its files live, how it is read, and how it is
//! generated.
//!
//! A port of the memory half of the reference's `sandbox/config.py`: `MemoryLayoutConfig`,
//! `MemoryReadConfig` and `MemoryGenerateConfig`. They are plain configuration, held here rather
//! than beside the capability because two layers read them: the memory capability in `ra-tools`
//! renders the read side into the instructions, and the runtime runs generation when a sandbox
//! session closes. Neither depends on the other; both depend on this.
//!
//! This is not [`crate::memory`]. That module is the retrieval contract a host's memory store
//! answers; sandbox memory is a set of files in the workspace that the model reads with its shell
//! and that a background pass writes.
//!
//! # Deviations from the reference
//!
//! - **Model instances are shared with `Arc`.** Cloning keeps their identity, as the reference
//!   does. Named models serialize as strings; serializing an in-memory model instance fails
//!   explicitly because a live model cannot be reconstructed from JSON.
//! - **A backslash in a layout directory is a separator.** The reference holds the directories as
//!   `Path`, so on a POSIX host `team\memory` names one directory with a backslash in its name, and
//!   its session addresses it that way because a typed path skips separator translation. Session
//!   paths here are text, and the session reads a backslash in text as a separator, as the
//!   reference's `coerce_posix_path` does for a string. [`MemoryLayoutConfig::memories_path`] and
//!   [`MemoryLayoutConfig::sessions_path`] read the directories that same way, and everything that
//!   touches memory files goes through them, so the files, the storage and the prompt name one
//!   directory.
//! - **Model settings are the framework's own.** The reference's default `Reasoning(effort=
//!   "medium")` is [`ModelSettings::with_effort`] with [`Effort::Medium`]. A dictionary of settings
//!   is read through the settings' own deserializer rather than coerced by a helper.

use std::{fmt, sync::Arc};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};

use crate::capability::Capability;
use crate::error::{Error, Result};
use crate::model::{Effort, Model, ModelSettings};
use crate::sandbox::PosixPath;

/// The directory consolidated memory files live in unless configured otherwise.
pub const DEFAULT_MEMORIES_DIR: &str = "memories";

/// The directory per-rollout JSONL files live in unless configured otherwise.
pub const DEFAULT_SESSIONS_DIR: &str = "sessions";

/// How many recent raw memories consolidation considers unless configured otherwise.
pub const DEFAULT_MAX_RAW_MEMORIES_FOR_CONSOLIDATION: u32 = 256;

/// The most raw memories consolidation may be configured to consider.
pub const MAX_RAW_MEMORIES_FOR_CONSOLIDATION_LIMIT: u32 = 4096;

/// The model phase-one extraction uses unless configured otherwise.
pub const DEFAULT_PHASE_ONE_MODEL: &str = "gpt-5.4-mini";

/// The model phase-two consolidation uses unless configured otherwise.
pub const DEFAULT_PHASE_TWO_MODEL: &str = "gpt-5.5";

/// Where sandbox memory keeps its files, relative to the workspace root.
///
/// The reference's `MemoryLayoutConfig`. The paths are checked by the capability that uses them,
/// as on the reference, not here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryLayoutConfig {
    memories_dir: String,
    sessions_dir: String,
}

impl Default for MemoryLayoutConfig {
    fn default() -> Self {
        Self {
            memories_dir: DEFAULT_MEMORIES_DIR.to_owned(),
            sessions_dir: DEFAULT_SESSIONS_DIR.to_owned(),
        }
    }
}

impl MemoryLayoutConfig {
    /// The default layout: `memories` and `sessions`, the names Codex uses.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Keeps consolidated memory files in `memories_dir`.
    #[must_use]
    pub fn with_memories_dir(mut self, memories_dir: impl Into<String>) -> Self {
        self.memories_dir = memories_dir.into();
        self
    }

    /// Keeps per-rollout JSONL files in `sessions_dir`.
    #[must_use]
    pub fn with_sessions_dir(mut self, sessions_dir: impl Into<String>) -> Self {
        self.sessions_dir = sessions_dir.into();
        self
    }

    /// The directory consolidated memory files live in, as configured.
    #[must_use]
    pub fn memories_dir(&self) -> &str {
        &self.memories_dir
    }

    /// The directory per-rollout JSONL files live in, as configured.
    #[must_use]
    pub fn sessions_dir(&self) -> &str {
        &self.sessions_dir
    }

    /// The directory consolidated memory files live in, as the session resolves it: a backslash
    /// is a separator.
    #[must_use]
    pub fn memories_path(&self) -> PosixPath {
        PosixPath::coerce(&self.memories_dir)
    }

    /// The directory per-rollout JSONL files live in, as the session resolves it: a backslash is a
    /// separator.
    #[must_use]
    pub fn sessions_path(&self) -> PosixPath {
        PosixPath::coerce(&self.sessions_dir)
    }
}

/// How sandbox memory is read during a run.
///
/// The reference's `MemoryReadConfig`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryReadConfig {
    live_update: bool,
}

impl Default for MemoryReadConfig {
    fn default() -> Self {
        Self { live_update: true }
    }
}

impl MemoryReadConfig {
    /// Reads memory and lets the agent update stale memory in place.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the agent may update stale memory files in place during a run.
    #[must_use]
    pub const fn with_live_update(mut self, live_update: bool) -> Self {
        self.live_update = live_update;
        self
    }

    /// Whether the agent may update stale memory files in place during a run.
    #[must_use]
    pub const fn live_update(&self) -> bool {
        self.live_update
    }
}

/// A phase model, either resolved by name or supplied as a shared live instance.
///
/// Instances compare by identity and retain that identity when the configuration is cloned.
#[non_exhaustive]
#[derive(Clone)]
pub enum MemoryModel {
    /// A name resolved by the run's model resolver.
    Named(String),
    /// A model used directly, without name resolution.
    Instance(Arc<dyn Model>),
}

impl MemoryModel {
    /// The configured name, when this is a named model.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        match self {
            Self::Named(name) => Some(name),
            Self::Instance(_) => None,
        }
    }

    /// The shared model, when supplied directly.
    #[must_use]
    pub fn instance(&self) -> Option<&Arc<dyn Model>> {
        match self {
            Self::Named(_) => None,
            Self::Instance(model) => Some(model),
        }
    }
}

impl fmt::Debug for MemoryModel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Named(name) => formatter.debug_tuple("Named").field(name).finish(),
            Self::Instance(_) => formatter.write_str("Instance(..)"),
        }
    }
}

impl PartialEq for MemoryModel {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Named(left), Self::Named(right)) => left == right,
            (Self::Instance(left), Self::Instance(right)) => Arc::ptr_eq(left, right),
            _ => false,
        }
    }
}

impl Eq for MemoryModel {}

impl From<String> for MemoryModel {
    fn from(name: String) -> Self {
        Self::Named(name)
    }
}

impl From<&str> for MemoryModel {
    fn from(name: &str) -> Self {
        Self::Named(name.to_owned())
    }
}

impl From<Arc<dyn Model>> for MemoryModel {
    fn from(model: Arc<dyn Model>) -> Self {
        Self::Instance(model)
    }
}

impl<T: Model + 'static> From<Arc<T>> for MemoryModel {
    fn from(model: Arc<T>) -> Self {
        Self::Instance(model)
    }
}

impl Serialize for MemoryModel {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        match self {
            Self::Named(name) => serializer.serialize_str(name),
            Self::Instance(_) => Err(serde::ser::Error::custom(
                "a live memory model instance cannot be serialized",
            )),
        }
    }
}

impl<'de> Deserialize<'de> for MemoryModel {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        String::deserialize(deserializer).map(Self::Named)
    }
}

/// How sandbox memory is generated when a sandbox session closes.
///
/// The reference's `MemoryGenerateConfig`: run segments are appended during the session, then each
/// rollout is extracted by phase one and everything is consolidated by phase two.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MemoryGenerateConfig {
    max_raw_memories_for_consolidation: u32,
    phase_one_model: MemoryModel,
    phase_one_model_settings: Option<ModelSettings>,
    phase_two_model: MemoryModel,
    phase_two_model_settings: Option<ModelSettings>,
    extra_prompt: Option<String>,
}

/// The reference's default settings for both phases: medium reasoning effort.
fn default_phase_model_settings() -> ModelSettings {
    ModelSettings::new().with_effort(Effort::Medium)
}

impl Default for MemoryGenerateConfig {
    fn default() -> Self {
        Self {
            max_raw_memories_for_consolidation: DEFAULT_MAX_RAW_MEMORIES_FOR_CONSOLIDATION,
            phase_one_model: DEFAULT_PHASE_ONE_MODEL.into(),
            phase_one_model_settings: Some(default_phase_model_settings()),
            phase_two_model: DEFAULT_PHASE_TWO_MODEL.into(),
            phase_two_model_settings: Some(default_phase_model_settings()),
            extra_prompt: None,
        }
    }
}

impl MemoryGenerateConfig {
    /// The reference's defaults.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Considers at most `limit` recent raw memories during consolidation.
    ///
    /// # Errors
    ///
    /// Returns a configuration error, worded as the reference's, for 0 or anything above 4096.
    pub fn with_max_raw_memories_for_consolidation(mut self, limit: u32) -> Result<Self> {
        validate_max_raw_memories(i64::from(limit))?;
        self.max_raw_memories_for_consolidation = limit;
        Ok(self)
    }

    /// Extracts each rollout with the model `model`.
    #[must_use]
    pub fn with_phase_one_model(mut self, model: impl Into<MemoryModel>) -> Self {
        self.phase_one_model = model.into();
        self
    }

    /// Extracts with `settings`, or with the model's own when `None`.
    #[must_use]
    pub fn with_phase_one_model_settings(mut self, settings: Option<ModelSettings>) -> Self {
        self.phase_one_model_settings = settings;
        self
    }

    /// Consolidates with the model `model`.
    #[must_use]
    pub fn with_phase_two_model(mut self, model: impl Into<MemoryModel>) -> Self {
        self.phase_two_model = model.into();
        self
    }

    /// Consolidates with `settings`, or with the model's own when `None`.
    #[must_use]
    pub fn with_phase_two_model_settings(mut self, settings: Option<ModelSettings>) -> Self {
        self.phase_two_model_settings = settings;
        self
    }

    /// Appends developer guidance to the extraction and consolidation prompts.
    ///
    /// Keep it short: phase one already receives a large built-in prompt and a truncated rollout
    /// in one context window, and oversized guidance crowds out the evidence it should summarize.
    #[must_use]
    pub fn with_extra_prompt(mut self, extra_prompt: Option<String>) -> Self {
        self.extra_prompt = extra_prompt;
        self
    }

    /// How many recent raw memories consolidation considers.
    #[must_use]
    pub const fn max_raw_memories_for_consolidation(&self) -> u32 {
        self.max_raw_memories_for_consolidation
    }

    /// The model phase one runs with.
    #[must_use]
    pub const fn phase_one_model(&self) -> &MemoryModel {
        &self.phase_one_model
    }

    /// The settings phase one runs with, or `None` for the model's own.
    #[must_use]
    pub const fn phase_one_model_settings(&self) -> Option<&ModelSettings> {
        self.phase_one_model_settings.as_ref()
    }

    /// The model phase two runs with.
    #[must_use]
    pub const fn phase_two_model(&self) -> &MemoryModel {
        &self.phase_two_model
    }

    /// The settings phase two runs with, or `None` for the model's own.
    #[must_use]
    pub const fn phase_two_model_settings(&self) -> Option<&ModelSettings> {
        self.phase_two_model_settings.as_ref()
    }

    /// The developer guidance appended to both prompts, if any.
    #[must_use]
    pub fn extra_prompt(&self) -> Option<&str> {
        self.extra_prompt.as_deref()
    }
}

/// The reference's bounds on `max_raw_memories_for_consolidation`, with its wording.
fn validate_max_raw_memories(limit: i64) -> Result<()> {
    if limit <= 0 {
        return Err(Error::config(
            "MemoryGenerateConfig.max_raw_memories_for_consolidation must be greater than 0.",
        ));
    }
    if limit > i64::from(MAX_RAW_MEMORIES_FOR_CONSOLIDATION_LIMIT) {
        return Err(Error::config(
            "MemoryGenerateConfig.max_raw_memories_for_consolidation must be less than or equal \
             to 4096.",
        ));
    }
    Ok(())
}

/// The serialized form, every field optional, before the reference's checks run.
#[derive(Deserialize)]
struct MemoryGenerateConfigFields {
    #[serde(default)]
    max_raw_memories_for_consolidation: Option<i64>,
    #[serde(default)]
    phase_one_model: Option<MemoryModel>,
    #[serde(default)]
    phase_one_model_settings: SettingsField,
    #[serde(default)]
    phase_two_model: Option<MemoryModel>,
    #[serde(default)]
    phase_two_model_settings: SettingsField,
    #[serde(default)]
    extra_prompt: Option<String>,
}

/// A phase's settings as serialized: left out, which keeps the default, or set — to `null`
/// included, which disables them.
#[derive(Default)]
enum SettingsField {
    #[default]
    Absent,
    Set(Option<Box<ModelSettings>>),
}

impl<'de> Deserialize<'de> for SettingsField {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        Option::<Box<ModelSettings>>::deserialize(deserializer).map(Self::Set)
    }
}

impl SettingsField {
    fn or(self, default: Option<ModelSettings>) -> Option<ModelSettings> {
        match self {
            Self::Absent => default,
            Self::Set(settings) => settings.map(|settings| *settings),
        }
    }
}

impl<'de> Deserialize<'de> for MemoryGenerateConfig {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let fields = MemoryGenerateConfigFields::deserialize(deserializer)?;
        let defaults = Self::default();
        if let Some(limit) = fields.max_raw_memories_for_consolidation {
            validate_max_raw_memories(limit).map_err(D::Error::custom)?;
        }
        Ok(Self {
            max_raw_memories_for_consolidation: fields
                .max_raw_memories_for_consolidation
                .and_then(|limit| u32::try_from(limit).ok())
                .unwrap_or(defaults.max_raw_memories_for_consolidation),
            phase_one_model: fields.phase_one_model.unwrap_or(defaults.phase_one_model),
            phase_one_model_settings: fields
                .phase_one_model_settings
                .or(defaults.phase_one_model_settings),
            phase_two_model: fields.phase_two_model.unwrap_or(defaults.phase_two_model),
            phase_two_model_settings: fields
                .phase_two_model_settings
                .or(defaults.phase_two_model_settings),
            extra_prompt: fields.extra_prompt,
        })
    }
}

/// What the sandbox memory capability tells the runtime about the memory it configures.
///
/// The reference's runtime reads the `Memory` capability it finds on a sandbox agent directly; here
/// the capability hands this over through
/// [`Capability::sandbox_memory`]. Besides the layout and generation configuration it carries the
/// capabilities the extraction and consolidation agents run with: the reference builds those
/// agents as sandbox agents, which get its default capability set, and that set lives with the
/// capabilities rather than with the runtime that runs them.
#[derive(Clone)]
pub struct SandboxMemory {
    layout: MemoryLayoutConfig,
    generate: Option<MemoryGenerateConfig>,
    phase_capabilities: Vec<Arc<dyn Capability>>,
}

impl SandboxMemory {
    /// Memory kept where `layout` says, generated as `generate` says or not at all when `None`,
    /// with extraction and consolidation running on `phase_capabilities`.
    #[must_use]
    pub fn new(
        layout: MemoryLayoutConfig,
        generate: Option<MemoryGenerateConfig>,
        phase_capabilities: Vec<Arc<dyn Capability>>,
    ) -> Self {
        Self {
            layout,
            generate,
            phase_capabilities,
        }
    }

    /// Where memory files are kept.
    #[must_use]
    pub const fn layout(&self) -> &MemoryLayoutConfig {
        &self.layout
    }

    /// How memory is generated, or `None` when it is not.
    #[must_use]
    pub const fn generate(&self) -> Option<&MemoryGenerateConfig> {
        self.generate.as_ref()
    }

    /// The capabilities the extraction and consolidation agents run with.
    #[must_use]
    pub fn phase_capabilities(&self) -> &[Arc<dyn Capability>] {
        &self.phase_capabilities
    }
}

impl fmt::Debug for SandboxMemory {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SandboxMemory")
            .field("layout", &self.layout)
            .field("generate", &self.generate)
            .field(
                "phase_capabilities",
                &self
                    .phase_capabilities
                    .iter()
                    .map(|capability| capability.kind())
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}
