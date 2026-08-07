//! Provider registration, `provider/model` parsing, and provider lifecycle management.
//!
//! [`ProviderRegistry`] is deliberately data-driven: names such as `openai`, `anthropic`,
//! `gemini`, `grok`, and `compat` have no special branches here. They become meaningful only when
//! an application installs matching [`ProviderRegistration`] values. An explicit registration
//! always wins over unknown-prefix forwarding.
//!
//! The registration is the ownership boundary for all provider-specific facts. Its factory owns
//! endpoint and credential configuration without exposing secrets through this registry, while
//! the registration itself owns aliases, protocol selection, model aliases, provider defaults,
//! and the static `extra_body` bucket. R1-6b must add `ProviderQuirks` to this same registration;
//! a second vendor table would make onboarding one endpoint a two-file operation.
//!
//! This follows the useful boundary of the `OpenAI` Agents SDK `MultiProvider`: a provider resolves
//! model names and owns cached connections, while the runner depends only on `Model`.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    sync::{Arc, Mutex, MutexGuard},
};

use async_trait::async_trait;
use ra_core::{
    error::{Error, Result},
    model::{
        ApiProtocol, JsonMap, Model, ModelProvider, ModelSettings, ProviderKey,
        ResolvedModelSettings,
    },
};

/// Lazily constructs one provider instance for a registration.
///
/// Concrete factories own endpoint, credentials, default headers, and client construction. That
/// state remains colocated with the registration and is intentionally absent from `Debug` output.
///
/// Factory work must be synchronous setup; network I/O belongs to the provider's model calls. It
/// also **must not call back into the registry that owns it** — construction runs while the
/// instance-cache lock is held, and that lock is not reentrant, so resolving another model from
/// inside a factory deadlocks rather than failing. Holding the lock across construction is
/// deliberate: it guarantees one instance per registration even under concurrent resolution, and
/// setup that cheap is not worth a second locking phase.
pub trait ProviderFactory: Send + Sync + 'static {
    /// Creates the provider instance cached by [`ProviderRegistry`].
    fn create(&self) -> Result<Arc<dyn ModelProvider>>;
}

impl<F> ProviderFactory for F
where
    F: Fn() -> Result<Arc<dyn ModelProvider>> + Send + Sync + 'static,
{
    fn create(&self) -> Result<Arc<dyn ModelProvider>> {
        (self)()
    }
}

/// A canonical provider model plus local aliases and resolved-model defaults.
///
/// The defaults are the model layer of the four-layer settings merge — typically this model's
/// output-token ceiling. Provider-wide mandatory fallbacks, such as the `max_tokens` Anthropic
/// requires on every request, belong to [`ProviderRegistration::with_defaults`]; a model that
/// states its own limit overrides that fallback, because it is the more specific of the two.
#[non_exhaustive]
#[derive(Clone)]
pub struct ModelRegistration {
    name: String,
    aliases: Vec<String>,
    defaults: ModelSettings,
}

impl ModelRegistration {
    /// Creates a canonical provider model registration.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            aliases: Vec::new(),
            defaults: ModelSettings::new(),
        }
    }

    /// Adds a local alias resolved before calling the provider.
    #[must_use]
    pub fn with_alias(mut self, alias: impl Into<String>) -> Self {
        self.aliases.push(alias.into());
        self
    }

    /// Sets this model's settings layer.
    ///
    /// For `max_tokens` this is read as a ceiling as well as a default: user intent from the agent
    /// or run layer overrides the number, but is then clamped by it. It does beat a provider-wide
    /// fallback, being the more specific registration of the two.
    #[must_use]
    pub fn with_defaults(mut self, defaults: ModelSettings) -> Self {
        self.defaults = defaults;
        self
    }

    /// Canonical model identifier sent to the provider.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Local names accepted for this model.
    #[must_use]
    pub fn aliases(&self) -> &[String] {
        &self.aliases
    }

    /// Resolved-model settings layer.
    #[must_use]
    pub const fn defaults(&self) -> &ModelSettings {
        &self.defaults
    }

    fn accepts(&self, name: &str) -> bool {
        self.name == name || self.aliases.iter().any(|alias| alias == name)
    }
}

impl fmt::Debug for ModelRegistration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ModelRegistration")
            .field("name", &self.name)
            .field("aliases", &self.aliases)
            .finish_non_exhaustive()
    }
}

/// One provider registration and every routing fact currently known about it.
///
/// The canonical [`ProviderKey`] is the identity used by `ModelSettings::extra_body`; aliases are
/// routing conveniences and never create new settings buckets. The factory, provider defaults,
/// and model registrations all live in this value so provider setup cannot drift across tables.
#[non_exhaustive]
#[derive(Clone)]
pub struct ProviderRegistration {
    key: ProviderKey,
    aliases: Vec<String>,
    protocol: ApiProtocol,
    factory: Arc<dyn ProviderFactory>,
    defaults: ModelSettings,
    models: Vec<ModelRegistration>,
}

impl ProviderRegistration {
    /// Starts a provider registration.
    #[must_use]
    pub fn new(key: ProviderKey, protocol: ApiProtocol, factory: impl ProviderFactory) -> Self {
        Self {
            key,
            aliases: Vec::new(),
            protocol,
            factory: Arc::new(factory),
            defaults: ModelSettings::new(),
            models: Vec::new(),
        }
    }

    /// Adds an accepted `provider/` prefix alias.
    #[must_use]
    pub fn with_alias(mut self, alias: impl Into<String>) -> Self {
        self.aliases.push(alias.into());
        self
    }

    /// Sets provider-registration defaults for the four-layer settings merge.
    ///
    /// Any `extra_body` bucket in this layer must use this registration's canonical key. Prefer
    /// [`Self::with_static_extra_body`] when only the static provider payload is being configured.
    #[must_use]
    pub fn with_defaults(mut self, defaults: ModelSettings) -> Self {
        self.defaults = defaults;
        self
    }

    /// Sets the static non-standard request-body fields required by this endpoint.
    ///
    /// The canonical provider key is applied automatically, preventing aliases from accidentally
    /// creating separate buckets.
    #[must_use]
    pub fn with_static_extra_body(mut self, body: JsonMap) -> Self {
        self.defaults = self.defaults.with_extra_body(self.key.clone(), body);
        self
    }

    /// Adds a canonical model and any local aliases.
    #[must_use]
    pub fn with_model(mut self, model: ModelRegistration) -> Self {
        self.models.push(model);
        self
    }

    /// Canonical provider registration identity.
    #[must_use]
    pub const fn key(&self) -> &ProviderKey {
        &self.key
    }

    /// Accepted provider-prefix aliases.
    #[must_use]
    pub fn aliases(&self) -> &[String] {
        &self.aliases
    }

    /// Wire protocol selected by this provider adapter.
    #[must_use]
    pub const fn protocol(&self) -> ApiProtocol {
        self.protocol
    }

    /// Provider-registration settings layer.
    #[must_use]
    pub const fn defaults(&self) -> &ModelSettings {
        &self.defaults
    }

    /// Static non-standard request-body fields for this provider.
    #[must_use]
    pub fn static_extra_body(&self) -> Option<&JsonMap> {
        self.defaults.extra_body().get(&self.key)
    }

    /// Registered canonical models and their aliases.
    #[must_use]
    pub fn models(&self) -> &[ModelRegistration] {
        &self.models
    }

    fn resolve_model(&self, name: Option<&str>) -> (Option<String>, ModelSettings) {
        let Some(name) = name else {
            return (None, ModelSettings::new());
        };

        self.models
            .iter()
            .find(|model| model.accepts(name))
            .map_or_else(
                || (Some(name.to_owned()), ModelSettings::new()),
                |model| (Some(model.name.clone()), model.defaults.clone()),
            )
    }
}

impl fmt::Debug for ProviderRegistration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderRegistration")
            .field("key", &self.key)
            .field("aliases", &self.aliases)
            .field("protocol", &self.protocol)
            .field("models", &self.models)
            .finish_non_exhaustive()
    }
}

/// Behavior for a namespaced model whose prefix has no explicit registration.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnknownPrefixPolicy {
    /// Reject the selector as a configuration error.
    Error,
    /// Forward the complete, unmodified selector to the named compatible provider.
    ForwardTo(ProviderKey),
}

/// Builder for an immutable [`ProviderRegistry`].
#[must_use]
pub struct ProviderRegistryBuilder {
    default_provider: ProviderKey,
    unknown_prefix_policy: UnknownPrefixPolicy,
    registrations: Vec<ProviderRegistration>,
}

impl ProviderRegistryBuilder {
    /// Creates a builder whose bare model names use `default_provider`.
    pub fn new(default_provider: ProviderKey) -> Self {
        Self {
            default_provider,
            unknown_prefix_policy: UnknownPrefixPolicy::Error,
            registrations: Vec::new(),
        }
    }

    /// Configures how unregistered prefixes are handled.
    pub fn unknown_prefix_policy(mut self, policy: UnknownPrefixPolicy) -> Self {
        self.unknown_prefix_policy = policy;
        self
    }

    /// Adds one provider registration.
    pub fn register(mut self, registration: ProviderRegistration) -> Self {
        self.registrations.push(registration);
        self
    }

    /// Validates registrations and creates the registry.
    pub fn build(self) -> Result<ProviderRegistry> {
        ProviderRegistry::from_builder(self)
    }
}

/// Parsed and canonicalized provider/model selection.
///
/// `provider` is always the canonical registration key, never the input alias. `model` is the
/// provider-facing model identifier after local alias resolution. A missing model delegates to the
/// provider's own default.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelSelector {
    provider: ProviderKey,
    model: Option<String>,
    protocol: ApiProtocol,
}

impl ModelSelector {
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

/// A selected model plus the two registration-owned settings layers needed by the runtime.
pub struct ResolvedModel {
    selector: ModelSelector,
    model: Arc<dyn Model>,
    provider_defaults: ModelSettings,
    model_defaults: ModelSettings,
}

impl ResolvedModel {
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

    /// Completes the four-layer immutable settings merge for this selected model.
    #[must_use]
    pub fn resolve_settings(
        &self,
        agent_defaults: &ModelSettings,
        run_overrides: &ModelSettings,
    ) -> ResolvedModelSettings {
        self.provider_defaults.resolve(
            self.selector.provider(),
            agent_defaults,
            &self.model_defaults,
            run_overrides,
        )
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

#[derive(Default)]
struct ProviderCache {
    closed: bool,
    instances: BTreeMap<ProviderKey, Arc<dyn ModelProvider>>,
}

/// Immutable provider registrations with lazy instance caching and unified shutdown.
///
/// Build a fresh registry to reopen providers after [`Self::close`]. Closing is idempotent, drains
/// every cached provider, deduplicates shared instances, and attempts all closes before returning
/// the first error.
pub struct ProviderRegistry {
    default_provider: ProviderKey,
    unknown_prefix_policy: UnknownPrefixPolicy,
    registrations: BTreeMap<ProviderKey, ProviderRegistration>,
    prefixes: BTreeMap<String, ProviderKey>,
    cache: Mutex<ProviderCache>,
}

impl ProviderRegistry {
    /// Starts a validated registry builder.
    pub fn builder(default_provider: ProviderKey) -> ProviderRegistryBuilder {
        ProviderRegistryBuilder::new(default_provider)
    }

    fn from_builder(builder: ProviderRegistryBuilder) -> Result<Self> {
        let mut registrations = BTreeMap::new();
        for registration in builder.registrations {
            validate_registration(&registration)?;
            let key = registration.key.clone();
            if registrations.insert(key.clone(), registration).is_some() {
                return Err(Error::config(format!(
                    "provider `{key}` was registered more than once"
                )));
            }
        }

        if !registrations.contains_key(&builder.default_provider) {
            return Err(Error::config(format!(
                "default provider `{}` is not registered",
                builder.default_provider
            )));
        }

        if let UnknownPrefixPolicy::ForwardTo(provider) = &builder.unknown_prefix_policy
            && !registrations.contains_key(provider)
        {
            return Err(Error::config(format!(
                "unknown-prefix target provider `{provider}` is not registered"
            )));
        }

        let mut prefixes = BTreeMap::new();
        for registration in registrations.values() {
            insert_prefix(&mut prefixes, registration.key.as_str(), &registration.key)?;
            for alias in &registration.aliases {
                insert_prefix(&mut prefixes, alias, &registration.key)?;
            }
        }

        Ok(Self {
            default_provider: builder.default_provider,
            unknown_prefix_policy: builder.unknown_prefix_policy,
            registrations,
            prefixes,
            cache: Mutex::new(ProviderCache::default()),
        })
    }

    /// Canonical provider used by bare model names and provider-default selection.
    #[must_use]
    pub const fn default_provider(&self) -> &ProviderKey {
        &self.default_provider
    }

    /// Configured unknown-prefix behavior.
    #[must_use]
    pub const fn unknown_prefix_policy(&self) -> &UnknownPrefixPolicy {
        &self.unknown_prefix_policy
    }

    /// Looks up a registration by canonical provider key.
    #[must_use]
    pub fn registration(&self, provider: &ProviderKey) -> Option<&ProviderRegistration> {
        self.registrations.get(provider)
    }

    /// Looks up a registration through either its canonical prefix or an alias.
    #[must_use]
    pub fn registration_for_prefix(&self, prefix: &str) -> Option<&ProviderRegistration> {
        self.prefixes
            .get(prefix)
            .and_then(|provider| self.registrations.get(provider))
    }

    /// Iterates over registrations in canonical key order.
    pub fn registrations(&self) -> impl Iterator<Item = &ProviderRegistration> {
        self.registrations.values()
    }

    /// Parses and canonicalizes a provider/model selector without instantiating a provider.
    pub fn select_model(&self, model_name: Option<&str>) -> Result<ModelSelector> {
        self.select_with_defaults(model_name)
            .map(|(selector, _)| selector)
    }

    /// Resolves a selector through a cached provider instance.
    pub fn resolve_model(&self, model_name: Option<&str>) -> Result<ResolvedModel> {
        let (selector, model_defaults) = self.select_with_defaults(model_name)?;
        let registration = self
            .registrations
            .get(selector.provider())
            .ok_or_else(|| Error::caller("selected provider registration disappeared"))?;
        let provider = self.provider_instance(registration)?;
        let model = provider.get_model(selector.model())?;

        Ok(ResolvedModel {
            selector,
            model,
            provider_defaults: registration.defaults.clone(),
            model_defaults,
        })
    }

    /// Closes every cached provider instance exactly once.
    pub async fn close(&self) -> Result<()> {
        let instances = {
            let mut cache = self.lock_cache()?;
            if cache.closed {
                return Ok(());
            }
            cache.closed = true;
            std::mem::take(&mut cache.instances)
        };

        let mut seen = BTreeSet::new();
        let mut first_error = None;
        for provider in instances.into_values() {
            let identity = Arc::as_ptr(&provider).cast::<()>() as usize;
            if seen.insert(identity)
                && let Err(error) = provider.close().await
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }

        first_error.map_or(Ok(()), Err)
    }

    fn select_with_defaults(
        &self,
        model_name: Option<&str>,
    ) -> Result<(ModelSelector, ModelSettings)> {
        let (provider, routed_model) = self.route(model_name)?;
        let registration = self
            .registrations
            .get(&provider)
            .ok_or_else(|| Error::caller("routed provider registration disappeared"))?;
        let (model, model_defaults) = registration.resolve_model(routed_model.as_deref());

        Ok((
            ModelSelector {
                provider,
                model,
                protocol: registration.protocol,
            },
            model_defaults,
        ))
    }

    fn route(&self, model_name: Option<&str>) -> Result<(ProviderKey, Option<String>)> {
        let Some(model_name) = model_name else {
            return Ok((self.default_provider.clone(), None));
        };
        if model_name.is_empty() {
            return Err(Error::config("model selector must not be empty"));
        }

        let Some((prefix, stripped_model)) = model_name.split_once('/') else {
            return Ok((self.default_provider.clone(), Some(model_name.to_owned())));
        };
        if prefix.is_empty() || stripped_model.is_empty() {
            return Err(Error::config(format!(
                "invalid model selector `{model_name}`; expected `provider/model`"
            )));
        }

        if let Some(provider) = self.prefixes.get(prefix) {
            return Ok((provider.clone(), Some(stripped_model.to_owned())));
        }

        match &self.unknown_prefix_policy {
            UnknownPrefixPolicy::Error => Err(Error::config(format!(
                "unknown model provider prefix `{prefix}`"
            ))),
            UnknownPrefixPolicy::ForwardTo(provider) => {
                Ok((provider.clone(), Some(model_name.to_owned())))
            }
        }
    }

    fn provider_instance(
        &self,
        registration: &ProviderRegistration,
    ) -> Result<Arc<dyn ModelProvider>> {
        let mut cache = self.lock_cache()?;
        if cache.closed {
            return Err(Error::caller(
                "provider registry is closed; build a fresh registry before resolving models",
            ));
        }
        if let Some(provider) = cache.instances.get(&registration.key) {
            return Ok(Arc::clone(provider));
        }

        let provider = registration.factory.create()?;
        cache
            .instances
            .insert(registration.key.clone(), Arc::clone(&provider));
        Ok(provider)
    }

    fn lock_cache(&self) -> Result<MutexGuard<'_, ProviderCache>> {
        self.cache
            .lock()
            .map_err(|_| Error::caller("provider registry cache lock is poisoned"))
    }
}

impl fmt::Debug for ProviderRegistry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderRegistry")
            .field("default_provider", &self.default_provider)
            .field("unknown_prefix_policy", &self.unknown_prefix_policy)
            .field("registrations", &self.registrations)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl ModelProvider for ProviderRegistry {
    fn get_model(&self, model_name: Option<&str>) -> Result<Arc<dyn Model>> {
        self.resolve_model(model_name)
            .map(ResolvedModel::into_model)
    }

    async fn close(&self) -> Result<()> {
        ProviderRegistry::close(self).await
    }
}

fn validate_registration(registration: &ProviderRegistration) -> Result<()> {
    validate_prefix(registration.key.as_str(), "provider key")?;
    for alias in &registration.aliases {
        validate_prefix(alias, "provider alias")?;
    }

    for key in registration.defaults.extra_body().keys() {
        if key != &registration.key {
            return Err(Error::config(format!(
                "provider `{}` defaults contain extra_body bucket `{key}`; use the canonical key",
                registration.key
            )));
        }
    }

    let mut model_names = BTreeSet::new();
    for model in &registration.models {
        validate_model_name(&model.name)?;
        insert_model_name(&mut model_names, &model.name, &registration.key)?;
        for alias in &model.aliases {
            validate_model_name(alias)?;
            insert_model_name(&mut model_names, alias, &registration.key)?;
        }
        for key in model.defaults.extra_body().keys() {
            if key != &registration.key {
                return Err(Error::config(format!(
                    "model `{}` defaults contain extra_body bucket `{key}`; expected `{}`",
                    model.name, registration.key
                )));
            }
        }
    }

    Ok(())
}

fn validate_prefix(prefix: &str, kind: &str) -> Result<()> {
    if prefix.is_empty() || prefix.trim() != prefix || prefix.contains('/') {
        return Err(Error::config(format!(
            "{kind} `{prefix}` must be non-empty, trimmed, and contain no `/`"
        )));
    }
    Ok(())
}

fn validate_model_name(name: &str) -> Result<()> {
    if name.is_empty() || name.trim() != name {
        return Err(Error::config(format!(
            "model name or alias `{name}` must be non-empty and trimmed"
        )));
    }
    Ok(())
}

fn insert_prefix(
    prefixes: &mut BTreeMap<String, ProviderKey>,
    prefix: &str,
    provider: &ProviderKey,
) -> Result<()> {
    if let Some(existing) = prefixes.insert(prefix.to_owned(), provider.clone()) {
        return Err(Error::config(format!(
            "provider prefix `{prefix}` is claimed by both `{existing}` and `{provider}`"
        )));
    }
    Ok(())
}

fn insert_model_name(
    names: &mut BTreeSet<String>,
    name: &str,
    provider: &ProviderKey,
) -> Result<()> {
    if !names.insert(name.to_owned()) {
        return Err(Error::config(format!(
            "model name or alias `{name}` is duplicated in provider `{provider}`"
        )));
    }
    Ok(())
}
