//! Configuration layering and provenance.
//!
//! # Mechanism only
//!
//! This module defines no **concrete configuration item**: those fields depend on each stage's
//! capabilities (provider, sandbox, context budget, and so on), are defined in their own stages,
//! and are carried by [`Layered<T>`]. What lives here is the machinery for "where a value came
//! from, what overrode what, and why something did not take effect". Reading files and environment
//! variables is likewise elsewhere: `ra-core` performs no IO, so this module only fixes the **path
//! and naming conventions** ([`CONFIG_DIR_NAME`], [`env_key`]) while discovery and parsing belong
//! to `ra-cli`.
//!
//! # Six sources, lowest to highest precedence
//!
//! | Source | Written by | Typical location |
//! | --- | --- | --- |
//! | [`SettingSource::Builtin`] | the framework | defaults in code |
//! | [`SettingSource::UserFile`] | the user | `~/.rusty-agent/config.toml` |
//! | [`SettingSource::ProjectFile`] | the team | `<project>/.rusty-agent/config.toml`, committed |
//! | [`SettingSource::LocalFile`] | one person | `<project>/.rusty-agent/config.local.toml`, **not committed** |
//! | [`SettingSource::Env`] | ops / CI | `RA_MODEL_NAME=...` |
//! | [`SettingSource::Explicit`] | the caller | CLI flags, API arguments |
//!
//! The order "team config < personal config < environment < command line" tracks **how close a
//! source is to this particular run**: the closer to the call at hand, the more say it gets.
//!
//! # Two deliberate decisions
//!
//! **1. Nothing external is read by default.** [`SourceSelection::default`] is
//! [`SourceSelection::isolated`], so only `Builtin` and `Explicit` apply. When the framework is
//! embedded in someone else's process, quietly reading `~/.rusty-agent/config.toml` is
//! unacceptable — that is the host application's user directory, not ours. `ra-cli` opens things
//! up by calling [`SourceSelection::all`] explicitly. This mirrors claude's `setting_sources`:
//! **sources are selected, not discovered**.
//!
//! **2. Excluded sources stay visible in diagnostics.** The isolation switch (mirroring
//! `strict_mcp_config`) makes on-disk configuration inert, but if `doctor` showed only the
//! effective value the user would be stuck on "I clearly wrote that config, why is it ignored".
//! So [`FieldReport`] lists every layer and distinguishes [`LayerStatus::Shadowed`] (a higher
//! layer won) from [`LayerStatus::Excluded`] (the source was never enabled) — **the two kinds of
//! "did not take effect" have completely different fixes**.

use core::fmt;

/// Configuration directory name under the home directory and the project root (same name, told
/// apart by location).
pub const CONFIG_DIR_NAME: &str = ".rusty-agent";

/// Configuration file name.
pub const CONFIG_FILE_NAME: &str = "config.toml";

/// Personal override file name. **It belongs in `.gitignore`**: it exists for "on my machine I
/// want a different model", and committing it imposes one person's preference on the whole team.
pub const LOCAL_CONFIG_FILE_NAME: &str = "config.local.toml";

/// Environment variable prefix.
pub const ENV_PREFIX: &str = "RA_";

/// Maps a configuration path to an environment variable name: `model.name` -> `RA_MODEL_NAME`.
///
/// Dots and hyphens both fold to underscores, so **do not use hyphens in configuration path
/// segments**: `a-b` and `a.b` collide on the same environment variable.
#[must_use]
pub fn env_key(path: &str) -> String {
    let mut key = String::with_capacity(ENV_PREFIX.len() + path.len());
    key.push_str(ENV_PREFIX);
    for ch in path.chars() {
        match ch {
            '.' | '-' => key.push('_'),
            other => key.extend(other.to_uppercase()),
        }
    }
    key
}

// ---------------------------------------------------------------------------
// sources
// ---------------------------------------------------------------------------

/// Which layer a configuration value came from.
///
/// Precedence is **projected** from the variant ([`Self::precedence`]) rather than stored — the
/// same approach as `Recoverability`, which makes the two impossible to disagree.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SettingSource {
    /// The framework's built-in default. **Always enabled**: disabling it would leave no
    /// fallback at all.
    Builtin,
    /// User-level configuration file.
    UserFile,
    /// Project-level configuration file (committed, shared by the team).
    ProjectFile,
    /// Personal override file inside the project (not committed).
    LocalFile,
    /// Environment variable.
    Env,
    /// Passed explicitly by the caller: CLI flags, API arguments. **Always enabled**: it is the
    /// intent of this very call.
    Explicit,
}

impl SettingSource {
    /// Every source, lowest to highest precedence. `doctor` and the gates iterate over it.
    pub const ALL: &'static [Self] = &[
        Self::Builtin,
        Self::UserFile,
        Self::ProjectFile,
        Self::LocalFile,
        Self::Env,
        Self::Explicit,
    ];

    /// Precedence; higher wins. The numbers themselves mean nothing — only their order is a
    /// contract.
    ///
    /// The gaps exist so a future layer (say "defaults contributed by a plugin") can be inserted
    /// between two existing ones without disturbing their relative order.
    #[must_use]
    pub const fn precedence(self) -> u16 {
        match self {
            Self::Builtin => 0,
            Self::UserFile => 100,
            Self::ProjectFile => 200,
            Self::LocalFile => 300,
            Self::Env => 400,
            Self::Explicit => 500,
        }
    }

    /// Stable machine-readable identity. It appears in `doctor` output and in traces.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::UserFile => "user",
            Self::ProjectFile => "project",
            Self::LocalFile => "local",
            Self::Env => "env",
            Self::Explicit => "explicit",
        }
    }

    /// Whether this source is **discovered from disk or the environment**.
    ///
    /// The isolation switch turns off exactly this class. [`Self::Builtin`] and
    /// [`Self::Explicit`] are not discovered — one is a hard-coded fallback, the other is what the
    /// caller just passed in — so neither risks taking effect by accident.
    #[must_use]
    pub const fn is_discovered(self) -> bool {
        !matches!(self, Self::Builtin | Self::Explicit)
    }

    /// Whether it comes from a configuration file.
    #[must_use]
    pub const fn is_file(self) -> bool {
        matches!(self, Self::UserFile | Self::ProjectFile | Self::LocalFile)
    }
}

impl fmt::Display for SettingSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

// ---------------------------------------------------------------------------
// source selection
// ---------------------------------------------------------------------------

/// Which sources take part in resolution. Mirrors claude's `setting_sources`.
///
/// It only governs the four **discovered** tiers ([`SettingSource::is_discovered`]); `Builtin`
/// and `Explicit` are permanently enabled, so the absurd state of "nothing configured at all"
/// cannot arise.
///
/// ```ignore
/// let isolated = SourceSelection::isolated();     // built-in defaults plus caller input only
/// let usual = SourceSelection::all();             // what ra-cli uses
/// let custom = SourceSelection::isolated().with(SettingSource::Env); // CI: environment only
/// ```
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct SourceSelection {
    /// Bit set of enabled **discovered sources**. The two permanently enabled tiers occupy no
    /// bit; see [`bit`].
    discovered: u8,
}

/// A source's bit in the set. Permanently enabled sources return 0, which makes toggling them a
/// natural no-op instead of a special case repeated in every method.
const fn bit(source: SettingSource) -> u8 {
    match source {
        SettingSource::UserFile => 1 << 0,
        SettingSource::ProjectFile => 1 << 1,
        SettingSource::LocalFile => 1 << 2,
        SettingSource::Env => 1 << 3,
        SettingSource::Builtin | SettingSource::Explicit => 0,
    }
}

impl SourceSelection {
    /// Honors [`SettingSource::Builtin`] and [`SettingSource::Explicit`] only; disk and
    /// environment are entirely inert. Mirrors the isolation semantics of `strict_mcp_config`.
    ///
    /// This is the [default](Self::default).
    #[must_use]
    pub const fn isolated() -> Self {
        Self { discovered: 0 }
    }

    /// Enables every source. **`ra-cli` uses this explicitly**; library callers choose for
    /// themselves.
    #[must_use]
    pub const fn all() -> Self {
        Self {
            discovered: bit(SettingSource::UserFile)
                | bit(SettingSource::ProjectFile)
                | bit(SettingSource::LocalFile)
                | bit(SettingSource::Env),
        }
    }

    /// Enables one source. A no-op for the permanently enabled ones.
    #[must_use]
    pub const fn with(mut self, source: SettingSource) -> Self {
        self.discovered |= bit(source);
        self
    }

    /// Disables one source. A no-op for the permanently enabled ones:
    /// [`SettingSource::Builtin`] and [`SettingSource::Explicit`] **cannot be turned off**.
    #[must_use]
    pub const fn without(mut self, source: SettingSource) -> Self {
        self.discovered &= !bit(source);
        self
    }

    /// Whether this source takes part in resolution.
    #[must_use]
    pub const fn allows(self, source: SettingSource) -> bool {
        !source.is_discovered() || self.discovered & bit(source) != 0
    }

    /// Whether isolation mode is in effect: no discovered source applies.
    #[must_use]
    pub const fn is_isolated(self) -> bool {
        self.discovered == 0
    }
}

impl fmt::Debug for SourceSelection {
    /// Lists which sources are enabled. A derived `discovered: 5` helps no one diagnosing
    /// anything, and diagnosis is nearly the only place this type shows up.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SourceSelection(")?;
        if self.is_isolated() {
            f.write_str("isolated")?;
        } else {
            let mut first = true;
            for source in SettingSource::ALL.iter().filter(|s| s.is_discovered()) {
                if self.allows(*source) {
                    if !first {
                        f.write_str("+")?;
                    }
                    f.write_str(source.label())?;
                    first = false;
                }
            }
        }
        f.write_str(")")
    }
}

impl Default for SourceSelection {
    /// [`Self::isolated`] — **no external configuration is read by default**. See the module
    /// documentation for why.
    fn default() -> Self {
        Self::isolated()
    }
}

// ---------------------------------------------------------------------------
// values with provenance
// ---------------------------------------------------------------------------

/// A value together with the layer it came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Sourced<T> {
    source: SettingSource,
    value: T,
}

impl<T> Sourced<T> {
    /// Creates one.
    #[must_use]
    pub const fn new(source: SettingSource, value: T) -> Self {
        Self { source, value }
    }

    /// Source layer.
    #[must_use]
    pub const fn source(&self) -> SettingSource {
        self.source
    }

    /// The value.
    #[must_use]
    pub const fn value(&self) -> &T {
        &self.value
    }

    /// Drops the provenance and returns the value.
    #[must_use]
    pub fn into_value(self) -> T {
        self.value
    }
}

// ---------------------------------------------------------------------------
// layered values
// ---------------------------------------------------------------------------

/// The candidate values of one configuration item across layers.
///
/// At most one value per layer: calling [`set`](Self::set) again for the same source **replaces**
/// rather than appends (one file per layer, so a repeat can only mean the same layer was parsed
/// twice).
#[derive(Debug, Clone)]
pub struct Layered<T> {
    entries: Vec<Sourced<T>>,
}

impl<T> Layered<T> {
    /// An empty layered value: no layer has supplied one.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Creates one with a built-in default. Nearly every configuration item should have a
    /// fallback.
    #[must_use]
    pub fn builtin(value: T) -> Self {
        let mut layered = Self::new();
        layered.set(SettingSource::Builtin, value);
        layered
    }

    /// Writes the value for one layer, replacing whatever that layer held.
    pub fn set(&mut self, source: SettingSource, value: T) -> &mut Self {
        if let Some(existing) = self.entries.iter_mut().find(|e| e.source == source) {
            existing.value = value;
        } else {
            self.entries.push(Sourced::new(source, value));
        }
        self
    }

    /// Whether no layer has supplied a value.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Returns one layer's raw value, **ignoring precedence and source selection**. For
    /// diagnostics.
    #[must_use]
    pub fn get(&self, source: SettingSource) -> Option<&T> {
        self.entries
            .iter()
            .find(|e| e.source == source)
            .map(Sourced::value)
    }

    /// Resolves the effective value by precedence, skipping sources that are not enabled.
    #[must_use]
    pub fn resolve(&self, selection: SourceSelection) -> Option<Sourced<&T>> {
        self.entries
            .iter()
            .filter(|e| selection.allows(e.source))
            .max_by_key(|e| e.source.precedence())
            .map(|e| Sourced::new(e.source, &e.value))
    }

    /// The effective value itself. Use [`Self::resolve`] to learn which layer it came from.
    #[must_use]
    pub fn value(&self, selection: SourceSelection) -> Option<&T> {
        self.resolve(selection).map(|s| *s.value())
    }

    /// The contributing layers, highest precedence first.
    pub fn sources(&self) -> impl Iterator<Item = SettingSource> {
        let mut sources: Vec<SettingSource> = self.entries.iter().map(Sourced::source).collect();
        sources.sort_unstable_by_key(|s| core::cmp::Reverse(s.precedence()));
        sources.into_iter()
    }
}

impl<T: fmt::Display> Layered<T> {
    /// Produces the diagnostic report that backs `ra doctor config`.
    ///
    /// Values are stringified here so the report can line up items of different types in one
    /// table.
    #[must_use]
    pub fn report(&self, key: impl Into<String>, selection: SourceSelection) -> FieldReport {
        let effective = self.resolve(selection).map(|s| s.source());

        let mut layers: Vec<LayerReport> = self
            .entries
            .iter()
            .map(|entry| LayerReport {
                source: entry.source,
                value: entry.value.to_string(),
                status: if !selection.allows(entry.source) {
                    LayerStatus::Excluded
                } else if Some(entry.source) == effective {
                    LayerStatus::Effective
                } else {
                    LayerStatus::Shadowed
                },
            })
            .collect();
        layers.sort_unstable_by_key(|l| core::cmp::Reverse(l.source.precedence()));

        FieldReport {
            key: key.into(),
            layers,
        }
    }
}

impl<T> Default for Layered<T> {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// diagnostics
// ---------------------------------------------------------------------------

/// Why one layer's value took effect, or why it did not.
///
/// Separating "was shadowed" from "was never enabled" is half the reason this module exists: the
/// first is fixed by changing a higher layer's configuration, the second by changing the source
/// selection. **The fixes have nothing in common.**
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LayerStatus {
    /// This layer's value is the effective one.
    Effective,
    /// Shadowed by a layer with higher precedence.
    Shadowed,
    /// The source is not enabled and never took part in resolution.
    Excluded,
}

impl LayerStatus {
    /// Stable machine-readable identity.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Effective => "effective",
            Self::Shadowed => "shadowed",
            Self::Excluded => "excluded",
        }
    }
}

impl fmt::Display for LayerStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// One layer's row in the diagnostic report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayerReport {
    source: SettingSource,
    value: String,
    status: LayerStatus,
}

impl LayerReport {
    /// Source layer.
    #[must_use]
    pub const fn source(&self) -> SettingSource {
        self.source
    }

    /// That layer's value, stringified.
    #[must_use]
    pub fn value(&self) -> &str {
        &self.value
    }

    /// Whether and how it applied.
    #[must_use]
    pub const fn status(&self) -> LayerStatus {
        self.status
    }
}

/// The full provenance report for one configuration item, one per line of `ra doctor config`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldReport {
    key: String,
    layers: Vec<LayerReport>,
}

impl FieldReport {
    /// Configuration path, such as `model.name`.
    #[must_use]
    pub fn key(&self) -> &str {
        &self.key
    }

    /// Every layer that supplied a value, highest precedence first. Disabled layers are included
    /// — **you cannot diagnose what you cannot see**.
    #[must_use]
    pub fn layers(&self) -> &[LayerReport] {
        &self.layers
    }

    /// The layer that won. `None` when no layer supplied a value, or when every layer that did is
    /// disabled.
    #[must_use]
    pub fn effective(&self) -> Option<&LayerReport> {
        self.layers
            .iter()
            .find(|l| l.status == LayerStatus::Effective)
    }

    /// Whether some layer was "configured but inert" — `doctor` should call those items out.
    #[must_use]
    pub fn has_ineffective_layers(&self) -> bool {
        self.layers
            .iter()
            .any(|l| l.status != LayerStatus::Effective)
    }
}

impl fmt::Display for FieldReport {
    /// A one-line summary: `model.name = gpt-5 (env)`. Per-layer detail goes through
    /// [`FieldReport::layers`], and formatting is `ra-cli`'s job.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.effective() {
            Some(layer) => write!(f, "{} = {} ({})", self.key, layer.value, layer.source),
            None => write!(f, "{} = <未设置>", self.key),
        }
    }
}
