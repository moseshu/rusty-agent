//! Model-aware catalog configuration for the explicitly selected skills namespace.

mod windows;

use std::{num::NonZeroUsize, sync::Arc};

use ra_context::window::ContextWindowConfig;
use ra_core::{
    capability::{Capability, CapabilityFamily},
    context::RunContext,
    error::Result,
    tool::Tool,
};
use ra_tools::capability::SkillsNsCapability;

/// Product configuration for the upstream single-catalog prompt.
///
/// The resolved run model selects a window from the Codex catalog table, independently of the
/// Agents SDK compaction capacities. Lookup uses the longest model prefix, then retries a single
/// provider namespace. Named models without a match use the upstream 272,000-token window; only
/// binding contexts without a model name fall back to 8,000 characters. An explicit token limit
/// takes precedence over the model window.
/// Optional usage instructions are omitted by default.
#[derive(Debug, Clone, Default)]
pub struct SkillCatalogPromptConfig {
    context_windows: ContextWindowConfig,
    max_context_tokens: Option<NonZeroUsize>,
    include_skills_usage_instructions: bool,
}

impl SkillCatalogPromptConfig {
    /// Uses the Codex catalog context-window table with upstream catalog defaults.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Layers explicit host model-window overrides over the Codex catalog table.
    ///
    /// Only the explicit entries of `context_windows` are used; its SDK compaction defaults and
    /// threshold ratio do not affect skill catalogs. Override names retain the configuration's
    /// normalization: trim, lowercase, remove an `openai/` prefix, and ignore dots and hyphens.
    #[must_use]
    pub fn with_context_windows(mut self, context_windows: ContextWindowConfig) -> Self {
        self.context_windows = context_windows;
        self
    }

    /// Overrides the proportional metadata budget, with the upstream 10,000-token cap.
    #[must_use]
    pub fn with_max_context_tokens(mut self, max_context_tokens: NonZeroUsize) -> Self {
        self.max_context_tokens = Some(max_context_tokens);
        self
    }

    /// Includes the optional upstream How to use skills wording.
    #[must_use]
    pub fn with_usage_instructions(mut self) -> Self {
        self.include_skills_usage_instructions = true;
        self
    }

    fn context_window(&self, model: &str) -> u64 {
        let key = override_lookup_key(model);
        self.context_windows
            .context_windows()
            .iter()
            .find_map(|(name, window)| (override_lookup_key(name) == key).then_some(*window))
            .unwrap_or_else(|| windows::context_window(model))
    }
}

/// Model lookup against supplied descriptors for the external integration-test workspace.
#[cfg(feature = "test-api")]
#[doc(hidden)]
#[must_use]
pub fn model_context_window_for_candidates(model: &str, candidates: &[(&str, u64)]) -> u64 {
    windows::context_window_from_candidates(model, candidates)
}

// Keep the validated host overrides' existing normalization without consulting the SDK defaults.
fn override_lookup_key(model: &str) -> String {
    let normalized = model.trim().to_lowercase();
    normalized
        .strip_prefix("openai/")
        .unwrap_or(&normalized)
        .chars()
        .filter(|character| !matches!(character, '.' | '-'))
        .collect()
}

pub(crate) struct SkillCatalogCapability {
    inner: SkillsNsCapability,
    config: SkillCatalogPromptConfig,
}

impl SkillCatalogCapability {
    pub(crate) const fn new(inner: SkillsNsCapability, config: SkillCatalogPromptConfig) -> Self {
        Self { inner, config }
    }
}

impl Capability for SkillCatalogCapability {
    fn kind(&self) -> CapabilityFamily {
        self.inner.kind()
    }

    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.inner.tools()
    }

    fn bind(&self, context: &RunContext) -> Result<Option<Arc<dyn Capability>>> {
        let window = context
            .resolved_model_name()
            .map(|model| self.config.context_window(model))
            .map(|window| i64::try_from(window).unwrap_or(i64::MAX));
        let mut capability = self.inner.clone().with_catalog_context_window(window);
        if let Some(limit) = self.config.max_context_tokens {
            capability = capability.with_catalog_max_context_tokens(limit);
        }
        if self.config.include_skills_usage_instructions {
            capability = capability.with_skills_usage_instructions();
        }
        capability.bind(context)
    }
}
