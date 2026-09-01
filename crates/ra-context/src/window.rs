//! Model context-window lookup plus configuration overrides.
//!
//! A model name is a routing value, not a trustworthy declaration of its capacity. This module
//! therefore only returns a window for a built-in entry or an explicit host override. An unknown
//! model has no automatic compaction threshold; guessing one would make a provider rejection look
//! like a context-management decision.
//!
//! The built-in entries and their lookup normalization follow the `OpenAI` Agents SDK compaction
//! capability. The threshold is intentionally different: this crate starts compaction at 60% of
//! a known window, leaving room for the request framing and the next model response.

use std::{collections::BTreeMap, fmt, sync::LazyLock};

use ra_core::error::{Error, Result};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};

/// The proportion of a model window at which compaction starts by default.
pub const DEFAULT_COMPACTION_THRESHOLD_RATIO: ContextWindowThresholdRatio =
    ContextWindowThresholdRatio::from_basis_points(6_000);

const BASIS_POINTS_PER_WHOLE: u16 = 10_000;

/// Slack for the binary rounding error carried by `ratio * BASIS_POINTS_PER_WHOLE`.
///
/// A decimal such as `0.56` has no exact binary form, so the scaled product misses a whole number
/// by up to 9.1e-13 across the range. A fifth decimal place, in contrast, moves the product at
/// least 0.1 away from a whole number. The tolerance sits between the two, far enough above the
/// representation noise that no four-decimal value is rejected.
const BASIS_POINT_ROUNDING_TOLERANCE: f64 = 1e-9;

/// A validated fraction of a context window.
///
/// It serializes as a decimal number in the inclusive range `0.0..=1.0`, so configuration can
/// express values such as `0.6` without using floating point for threshold arithmetic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ContextWindowThresholdRatio(u16);

impl ContextWindowThresholdRatio {
    /// Creates a ratio from basis points, where 10,000 means the complete context window.
    pub fn new(basis_points: u16) -> Result<Self> {
        if basis_points > BASIS_POINTS_PER_WHOLE {
            return Err(Error::config(format!(
                "a context-window threshold ratio must be between 0 and 1, not {}",
                f64::from(basis_points) / f64::from(BASIS_POINTS_PER_WHOLE)
            )));
        }
        Ok(Self(basis_points))
    }

    /// Creates a ratio from a compile-time validated basis-point value.
    #[must_use]
    pub const fn from_basis_points(basis_points: u16) -> Self {
        assert!(basis_points <= BASIS_POINTS_PER_WHOLE);
        Self(basis_points)
    }

    /// Ratio in basis points, where 10,000 means the complete context window.
    #[must_use]
    pub const fn basis_points(self) -> u16 {
        self.0
    }

    /// Calculates the integer threshold for one context window.
    #[must_use]
    pub const fn apply(self, context_window: u64) -> u64 {
        let scale = BASIS_POINTS_PER_WHOLE as u64;
        let ratio = self.0 as u64;
        let whole = context_window / scale;
        let remainder = context_window % scale;
        whole * ratio + remainder * ratio / scale
    }
}

impl Default for ContextWindowThresholdRatio {
    fn default() -> Self {
        DEFAULT_COMPACTION_THRESHOLD_RATIO
    }
}

impl Serialize for ContextWindowThresholdRatio {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_f64(f64::from(self.0) / f64::from(BASIS_POINTS_PER_WHOLE))
    }
}

impl<'de> Deserialize<'de> for ContextWindowThresholdRatio {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let ratio = f64::deserialize(deserializer)?;
        if !ratio.is_finite() || !(0.0..=1.0).contains(&ratio) {
            return Err(D::Error::custom(
                "a context-window threshold ratio must be a finite number between 0 and 1",
            ));
        }

        let scaled = ratio * f64::from(BASIS_POINTS_PER_WHOLE);
        let rounded = scaled.round();
        if (scaled - rounded).abs() > BASIS_POINT_ROUNDING_TOLERANCE {
            return Err(D::Error::custom(
                "a context-window threshold ratio supports at most four decimal places",
            ));
        }

        // The range check bounds `rounded` to `0.0..=10_000.0` and the tolerance check proves it
        // is a whole number, so the conversion loses nothing.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let basis_points = rounded as u16;
        Self::new(basis_points).map_err(D::Error::custom)
    }
}

/// The single lookup path from a model identifier to its token window.
///
/// It resolves host overrides ahead of the shipped entries. Both sides are keyed by the same
/// normalization, so a name is normalized exactly once per lookup.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContextWindowTable {
    overrides: BTreeMap<String, u64>,
}

impl ContextWindowTable {
    /// Validates host-owned overrides and layers them over the shipped entries.
    ///
    /// Overrides replace a built-in entry with the same normalized name and can add an otherwise
    /// unknown model. Two distinct configuration names that normalize to the same key are refused
    /// instead of letting map ordering decide which window is used.
    pub fn with_overrides(overrides: &BTreeMap<String, u64>) -> Result<Self> {
        let mut normalized = BTreeMap::new();

        for (model, context_window) in overrides {
            let key = model_lookup_key(model);
            if key.is_empty() {
                return Err(Error::config(
                    "a context-window override model name cannot be empty",
                ));
            }
            if *context_window == 0 {
                return Err(Error::config(format!(
                    "the context window for model `{model}` must be at least one token"
                )));
            }
            if normalized.insert(key.clone(), *context_window).is_some() {
                return Err(Error::config(format!(
                    "multiple context-window overrides normalize to model `{key}`"
                )));
            }
        }

        Ok(Self {
            overrides: normalized,
        })
    }

    /// Looks up a model's context window after canonical normalization.
    #[must_use]
    pub fn context_window(&self, model: &str) -> Option<u64> {
        let key = model_lookup_key(model);
        self.overrides
            .get(&key)
            .or_else(|| BUILT_IN_WINDOWS.get(&key))
            .copied()
    }
}

/// Serializable context-window settings owned by the host configuration.
///
/// `context_windows` is deliberately an override map rather than a replacement table: built-in
/// entries evolve with the framework, while an endpoint can correct or extend only the models it
/// owns. Use an explicit entry for an unknown provider model before enabling automatic compaction.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ContextWindowConfig {
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    context_windows: BTreeMap<String, u64>,
    compaction_threshold_ratio: ContextWindowThresholdRatio,
    /// Resolved once at construction: every lookup goes through it, and it is a pure function of
    /// `context_windows`, so it is rebuilt on deserialization rather than carried in the document.
    #[serde(skip)]
    table: ContextWindowTable,
}

impl ContextWindowConfig {
    /// Creates validated host configuration from model-window overrides and a trigger ratio.
    pub fn new(
        context_windows: BTreeMap<String, u64>,
        compaction_threshold_ratio: ContextWindowThresholdRatio,
    ) -> Result<Self> {
        let table = ContextWindowTable::with_overrides(&context_windows)?;
        Ok(Self {
            context_windows,
            compaction_threshold_ratio,
            table,
        })
    }

    /// Host overrides before normalization and application to the built-in table.
    #[must_use]
    pub const fn context_windows(&self) -> &BTreeMap<String, u64> {
        &self.context_windows
    }

    /// The fraction of a known window that triggers compaction.
    #[must_use]
    pub const fn compaction_threshold_ratio(&self) -> ContextWindowThresholdRatio {
        self.compaction_threshold_ratio
    }

    /// The resolved built-in-plus-host table.
    #[must_use]
    pub const fn table(&self) -> &ContextWindowTable {
        &self.table
    }

    /// Returns a model's configured window, if its capacity is known.
    #[must_use]
    pub fn context_window(&self, model: &str) -> Option<u64> {
        self.table.context_window(model)
    }

    /// Returns the proportional compaction trigger for a known model.
    #[must_use]
    pub fn compaction_threshold(&self, model: &str) -> Option<u64> {
        self.context_window(model)
            .map(|window| self.compaction_threshold_ratio.apply(window))
    }
}

impl<'de> Deserialize<'de> for ContextWindowConfig {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct RawContextWindowConfig {
            #[serde(default)]
            context_windows: BTreeMap<String, u64>,
            #[serde(default)]
            compaction_threshold_ratio: ContextWindowThresholdRatio,
        }

        let raw = RawContextWindowConfig::deserialize(deserializer)?;
        Self::new(raw.context_windows, raw.compaction_threshold_ratio).map_err(D::Error::custom)
    }
}

impl fmt::Display for ContextWindowThresholdRatio {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}",
            f64::from(self.0) / f64::from(BASIS_POINTS_PER_WHOLE)
        )
    }
}

fn model_lookup_key(model: &str) -> String {
    let normalized = model.trim().to_lowercase();
    normalized
        .strip_prefix("openai/")
        .unwrap_or(&normalized)
        .chars()
        .filter(|character| !matches!(character, '.' | '-'))
        .collect()
}

/// The shipped entries, keyed by normalized model name and built on first lookup.
static BUILT_IN_WINDOWS: LazyLock<BTreeMap<String, u64>> = LazyLock::new(built_in_windows);

// This is data copied from the reference table; splitting it would only separate model aliases
// from the capacity they share.
#[allow(clippy::too_many_lines)]
fn built_in_windows() -> BTreeMap<String, u64> {
    let mut windows = BTreeMap::new();
    for (models, context_window) in [
        (
            &[
                "gpt-5.4",
                "gpt-5.4-2026-03-05",
                "gpt-5.4-pro",
                "gpt-5.4-pro-2026-03-05",
                "gpt-5.5",
                "gpt-5.5-2026-04-23",
                "gpt-5.5-pro",
                "gpt-5.5-pro-2026-04-23",
                "gpt-5.6",
                "gpt-5.6-sol",
                "gpt-5.6-terra",
                "gpt-5.6-luna",
                "gpt-4.1",
                "gpt-4.1-2025-04-14",
                "gpt-4.1-mini",
                "gpt-4.1-mini-2025-04-14",
                "gpt-4.1-nano",
                "gpt-4.1-nano-2025-04-14",
            ][..],
            1_047_576,
        ),
        (
            &[
                "gpt-5",
                "gpt-5-2025-08-07",
                "gpt-5-codex",
                "gpt-5-mini",
                "gpt-5-mini-2025-08-07",
                "gpt-5-nano",
                "gpt-5-nano-2025-08-07",
                "gpt-5-pro",
                "gpt-5-pro-2025-10-06",
                "gpt-5.1",
                "gpt-5.1-2025-11-13",
                "gpt-5.1-codex",
                "gpt-5.1-codex-max",
                "gpt-5.1-codex-mini",
                "gpt-5.2",
                "gpt-5.2-2025-12-11",
                "gpt-5.2-codex",
                "gpt-5.2-pro",
                "gpt-5.2-pro-2025-12-11",
                "gpt-5.3-codex",
                "gpt-5.4-mini",
                "gpt-5.4-mini-2026-03-17",
                "gpt-5.4-nano",
                "gpt-5.4-nano-2026-03-17",
            ][..],
            400_000,
        ),
        (
            &[
                "codex-mini-latest",
                "o1",
                "o1-2024-12-17",
                "o1-pro",
                "o1-pro-2025-03-19",
                "o3",
                "o3-2025-04-16",
                "o3-deep-research",
                "o3-deep-research-2025-06-26",
                "o3-mini",
                "o3-mini-2025-01-31",
                "o3-pro",
                "o3-pro-2025-06-10",
                "o4-mini",
                "o4-mini-2025-04-16",
                "o4-mini-deep-research",
                "o4-mini-deep-research-2025-06-26",
            ][..],
            200_000,
        ),
        (
            &[
                "gpt-4o",
                "gpt-4o-2024-05-13",
                "gpt-4o-2024-08-06",
                "gpt-4o-2024-11-20",
                "gpt-4o-mini",
                "gpt-4o-mini-2024-07-18",
                "gpt-5-chat-latest",
                "gpt-5.1-chat-latest",
                "gpt-5.2-chat-latest",
                "gpt-5.3-chat-latest",
            ][..],
            128_000,
        ),
    ] {
        for model in models {
            let previous = windows.insert(model_lookup_key(model), context_window);
            debug_assert!(
                previous.is_none(),
                "built-in context-window keys must be unique"
            );
        }
    }
    windows
}
