//! Explicit Host discovery and model-aware configuration for skill catalogs.

mod config;
mod discovery;
mod metadata;
mod parser;
pub use metadata::SkillProduct;
mod windows;
pub use config::{SkillConfig, SkillConfigRule, SkillConfigRuleSelector, SkillConfigRules};
pub(crate) use discovery::HostSkillProvider;
pub use discovery::{PluginSkillRoot, SkillDiscoveryConfig, SkillDiscoveryMode};

use std::{num::NonZeroUsize, sync::Arc};

use ra_context::window::ContextWindowConfig;
use ra_core::{
    capability::{Capability, CapabilityFamily},
    context::RunContext,
    error::Result,
    tool::Tool,
};
use ra_tools::capability::SkillsNsCapability;

/// Product configuration for the upstream skill catalogs and their shared metadata budget.
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
    required_skills: Vec<(String, Vec<String>)>,
}

impl SkillCatalogPromptConfig {
    pub(crate) fn has_requirements(&self) -> bool {
        !self.required_skills.is_empty()
    }
    /// Requires exact skill names from a selected environment before inference.
    /// Requirements are scoped by the main resource's environment binding, including Cloud
    /// resources. They do not select or read skills. Replaces this environment's previous list.
    /// Selection order is retained; an empty list removes the environment requirement.
    #[must_use]
    pub fn with_required_skills(
        mut self,
        environment_id: impl Into<String>,
        skill_names: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        ra_tools::skills_ns::set_required_skills(
            &mut self.required_skills,
            environment_id.into(),
            skill_names.into_iter().map(Into::into).collect(),
        );
        self
    }
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
        for (environment, names) in &self.config.required_skills {
            capability = capability.with_required_skills(environment, names.iter().cloned());
        }
        capability.bind(context)
    }
}

/// How the product selects skill tools; Host catalogs use existing filesystem tools.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SkillToolSurface {
    Merged,
    Namespace,
    Host,
}
