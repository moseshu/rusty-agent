//! Protocol-neutral result of provider and model-name resolution.

use std::{fmt, sync::Arc};

use super::{
    ApiProtocol, Model, ModelSettings, ProviderKey, ResolvedModelSettings, SettingsLayers,
};
use crate::error::{Error, Result};

/// Canonical provider/model selection produced by a model resolver.
///
/// `provider` is a registration identity rather than a vendor enum. `model` is the
/// provider-facing name after alias resolution, and `None` delegates to that provider's default.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelSelector {
    provider: ProviderKey,
    model: Option<String>,
    protocol: ApiProtocol,
}

impl ModelSelector {
    /// Creates a canonical selection from resolver-owned facts.
    #[must_use]
    pub fn new(provider: ProviderKey, model: Option<String>, protocol: ApiProtocol) -> Self {
        Self {
            provider,
            model,
            protocol,
        }
    }

    /// Canonical provider registration key.
    #[must_use]
    pub const fn provider(&self) -> &ProviderKey {
        &self.provider
    }

    /// Provider-facing model identifier, or `None` for the provider default.
    #[must_use]
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    /// Wire protocol selected by the provider registration.
    #[must_use]
    pub const fn protocol(&self) -> ApiProtocol {
        self.protocol
    }
}

/// A selected model plus the registration-owned settings layers needed by the runtime.
pub struct ResolvedModel {
    selector: ModelSelector,
    model: Arc<dyn Model>,
    provider_defaults: ModelSettings,
    model_defaults: ModelSettings,
    output_token_limit: Option<u64>,
}

impl ResolvedModel {
    /// Creates a resolution from provider-registry output.
    #[must_use]
    pub fn new(
        selector: ModelSelector,
        model: Arc<dyn Model>,
        provider_defaults: ModelSettings,
        model_defaults: ModelSettings,
    ) -> Self {
        Self {
            selector,
            model,
            provider_defaults,
            model_defaults,
            output_token_limit: None,
        }
    }

    /// Sets the selected model's hard output-token limit.
    #[must_use]
    pub const fn with_output_token_limit(mut self, limit: u64) -> Self {
        self.output_token_limit = Some(limit);
        self
    }

    /// Canonical provider/model selection used for this instance.
    #[must_use]
    pub const fn selector(&self) -> &ModelSelector {
        &self.selector
    }

    /// Resolved model trait object.
    #[must_use]
    pub const fn model(&self) -> &Arc<dyn Model> {
        &self.model
    }

    /// Provider-registration settings layer.
    #[must_use]
    pub const fn provider_defaults(&self) -> &ModelSettings {
        &self.provider_defaults
    }

    /// Resolved-model settings layer.
    #[must_use]
    pub const fn model_defaults(&self) -> &ModelSettings {
        &self.model_defaults
    }

    /// Selected model's hard output-token limit, when the resolver knows it.
    #[must_use]
    pub const fn output_token_limit(&self) -> Option<u64> {
        self.output_token_limit
    }

    /// Completes the four-layer immutable settings merge for this selected model.
    #[must_use]
    pub fn resolve_settings(
        &self,
        agent_defaults: &ModelSettings,
        run_overrides: &ModelSettings,
    ) -> ResolvedModelSettings {
        SettingsLayers::new()
            .with_provider(&self.provider_defaults)
            .with_model(&self.model_defaults)
            .with_agent(agent_defaults)
            .with_run(run_overrides)
            .resolve(self.selector.provider())
    }

    /// Rejects a resolved request setting that exceeds the selected model's hard limit.
    pub fn validate_settings(&self, settings: &ResolvedModelSettings) -> Result<()> {
        if let (Some(requested), Some(limit)) = (settings.max_tokens(), self.output_token_limit)
            && requested > limit
        {
            return Err(Error::config(format!(
                "max_tokens ({requested}) exceeds the selected model's output limit ({limit})"
            )));
        }
        Ok(())
    }

    /// Consumes the resolution and returns the model trait object.
    #[must_use]
    pub fn into_model(self) -> Arc<dyn Model> {
        self.model
    }
}

impl fmt::Debug for ResolvedModel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResolvedModel")
            .field("selector", &self.selector)
            .finish_non_exhaustive()
    }
}
