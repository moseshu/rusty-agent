// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Resolve a product model against its catalog and build the selected Responses provider.

use std::{
    sync::{Arc, LazyLock},
    time::Duration,
};

use http::{HeaderMap, HeaderName, HeaderValue};
use ra_config::{ModelProviderInfo, Personality, ReasoningEffort, ReasoningSummary, WireApi};
use ra_context::{
    compaction::CompactionCapability,
    window::{ContextWindowConfig, ContextWindowThresholdRatio},
};
use ra_core::{
    capability::{
        Capability, CapabilityFamily, ContextProcessor, ContextProcessorRequest,
        ContextProcessorResult, ContextSummarizer,
    },
    error::{Error, Result},
    filter::{ContextFilter, ContextFilterRequest, ModelInputData},
    item::{Message, ModelInputItem},
    model::{
        ApiProtocol, ModelProvider, ModelResolver, ModelRetrySettings, ModelSettings, ProviderKey,
        ResolvedModel,
    },
    sandbox::token_truncation::{
        TruncationPolicy, approx_bytes_for_tokens, approx_token_count, truncate_text,
    },
    tool::{ToolOutput, ToolOutputBlock},
};
use ra_http::{ClientRouteClass, RouteAwareClientPool};
use ra_model::{
    openai::{
        auth::OpenAiAuth,
        responses::{OpenAiResponsesProvider, ResponsesRequestSigner, developer_prefix},
    },
    provider::{ProviderRegistration, ProviderRegistry, quirks::ProviderQuirks},
};
use ra_runtime::runner::RunConfig;
use serde_json::{Map, json};

use super::{
    Config, ModelInfo, ModelsResponse, amazon_bedrock,
    catalog::{ModelVisibility, TruncationMode, find_model_by_slug},
    model_auth::CommandBearerToken,
    model_catalog_remote::{RemoteModelCatalog, RemoteModelCatalogs},
    support::forwards_configured_response_metadata,
};

const OPENAI_BASE_URL: &str = "https://api.openai.com/v1";
const RESIDENCY_HEADER_NAME: &str = "x-openai-internal-codex-residency";
const BASE_INSTRUCTIONS: &str = include_str!("model_prompt.md");

static BUNDLED_MODELS: LazyLock<std::result::Result<ModelsResponse, serde_json::Error>> =
    LazyLock::new(|| serde_json::from_str(include_str!("models.json")));

/// The bundled `OpenAI` catalog that provider catalogs derive from.
#[cfg(feature = "test-api")]
pub(super) fn bundled_models() -> Option<&'static ModelsResponse> {
    BUNDLED_MODELS.as_ref().ok()
}

pub(crate) struct ProductModelRuntime {
    config: Arc<Config>,
    registry: Option<ProviderRegistry>,
    explicit: Option<Arc<dyn ModelResolver>>,
    default_model: Option<String>,
    remote: Option<Arc<RemoteModelCatalog>>,
    /// Provider-owned static catalog, such as Bedrock's, used in place of the bundled one.
    provider_catalog: Option<Arc<ModelsResponse>>,
}

struct TurnCatalog {
    source: Arc<RemoteModelCatalog>,
    models: Option<Arc<ModelsResponse>>,
}

impl ProductModelRuntime {
    pub(crate) async fn new(
        config: Arc<Config>,
        explicit: Option<Arc<dyn ModelResolver>>,
        initial_model: Option<&str>,
        offline: bool,
        catalogs: &RemoteModelCatalogs,
    ) -> Result<Self> {
        let bundled = BUNDLED_MODELS
            .as_ref()
            .map_err(|error| Error::config(format!("invalid bundled model catalog: {error}")))?;
        let mut prepared_auth = None;
        let mut signer: Option<Arc<dyn ResponsesRequestSigner>> = None;
        let mut provider_catalog = None;
        if explicit.is_none() && config.model_provider.is_amazon_bedrock() {
            let (auth, bedrock_signer, catalog) = prepare_bedrock(&config, bundled).await?;
            prepared_auth = Some((auth, None));
            signer = Some(bedrock_signer);
            provider_catalog = Some(catalog);
        }
        let remote = if explicit.is_none()
            && config.model_catalog.is_none()
            && let Some(url) = &config.model_provider.model_catalog_url
        {
            let (auth, key) = provider_auth(
                &config.model_provider,
                config.model_provider_id == ra_config::model_provider::OPENAI_PROVIDER_ID,
                config.enforce_residency.value(),
            )?;
            let command = config
                .model_provider
                .auth
                .clone()
                .map(CommandBearerToken::new)
                .map(Arc::new);
            let api_key_disabled = command.is_none()
                && key.is_some()
                && !config
                    .features
                    .enabled(ra_config::Feature::ApiKeyModelDiscovery);
            let directory = catalogs.share(
                RemoteModelCatalog::new(
                    &config,
                    url.as_str(),
                    &auth,
                    key,
                    command,
                    api_key_disabled,
                )
                .map_err(Error::config)?,
            );
            directory.load(offline).await;
            prepared_auth = Some((auth, directory.command()));
            Some(directory)
        } else {
            None
        };
        let remote_snapshot = remote.as_ref().and_then(|catalog| catalog.snapshot());
        let models = match &remote {
            Some(_) => remote_snapshot.as_deref(),
            None => Some(
                provider_catalog
                    .as_deref()
                    .or(config.model_catalog.as_ref())
                    .unwrap_or(bundled),
            ),
        };
        let requested = remote
            .as_ref()
            .and(initial_model)
            .map(str::to_owned)
            .or_else(|| config.model.clone());
        let default_model = requested.or_else(|| {
            if explicit.is_some() {
                return None;
            }
            models?
                .models
                .iter()
                .filter(|model| model.supported_in_api)
                .min_by_key(|model| (model.visibility != ModelVisibility::List, model.priority))
                .map(|model| model.slug.clone())
        });
        let registry = if explicit.is_none() {
            let default_model = default_model.as_deref().filter(|model| !model.trim().is_empty())
                .ok_or_else(|| Error::config(
                    "No models are available. Set `model` explicitly or check your model catalog configuration."
                ))?.to_owned();
            Some(build_registry(
                &config,
                default_model,
                remote.clone(),
                prepared_auth,
                signer,
            )?)
        } else {
            None
        };

        Ok(Self {
            config,
            registry,
            explicit,
            default_model,
            remote,
            provider_catalog,
        })
    }

    pub(crate) fn configure_run(&self, config: RunConfig) -> RunConfig {
        let Some(source) = &self.remote else {
            return config;
        };
        let source = Arc::clone(source);
        config.with_run_snapshot(move || {
            let source = Arc::clone(&source);
            async move {
                source.refresh_after_auth_change().await;
                let models = source.snapshot();
                TurnCatalog { source, models }
            }
        })
    }

    fn model_info(&self, model: &str) -> Option<ModelInfo> {
        if let Some(remote) = &self.remote {
            let models = match RunConfig::current_run_snapshot::<TurnCatalog>() {
                Some(snapshot) if Arc::ptr_eq(&snapshot.source, remote) => snapshot.models.clone(),
                _ => remote.snapshot(),
            };
            return models?
                .models
                .iter()
                .find(|candidate| candidate.slug == model)
                .cloned();
        }
        let catalog = self
            .provider_catalog
            .as_deref()
            .or(self.config.model_catalog.as_ref())
            .or_else(|| BUNDLED_MODELS.as_ref().ok())?;
        let candidates = catalog
            .models
            .iter()
            .map(|info| (info.slug.as_str(), info))
            .collect::<Vec<_>>();
        find_model_by_slug(model, &candidates).cloned()
    }

    fn product_settings(&self, model: &str, provider: &ProviderKey) -> ModelSettings {
        let info = self.model_info(model);
        let info = info.as_ref();
        let mut settings = ModelSettings::new();
        let mut reasoning = Map::new();
        if let Some(effort) = self
            .config
            .model_reasoning_effort
            .as_ref()
            .or_else(|| info.and_then(|info| info.default_reasoning_level.as_ref()))
        {
            let effort = resolve_reasoning_effort(info, effort);
            reasoning.insert("effort".to_owned(), json!(effort.as_str()));
        }
        if info.is_none_or(|info| info.supports_reasoning_summary_parameter) {
            let summary = self.config.model_reasoning_summary.unwrap_or_else(|| {
                info.map_or(ReasoningSummary::Auto, |info| {
                    info.default_reasoning_summary
                })
            });
            if summary != ReasoningSummary::None {
                reasoning.insert("summary".to_owned(), json!(summary));
            }
        }
        if info.is_some_and(|info| info.use_responses_lite) {
            reasoning.insert("context".to_owned(), json!("all_turns"));
            settings = settings.with_parallel_tool_calls(false);
        }
        settings = settings.with_extra_body_value(provider.clone(), "reasoning", json!(reasoning));

        if let Some(verbosity) = info
            .filter(|info| info.support_verbosity)
            .and_then(|info| self.config.model_verbosity.or(info.default_verbosity))
        {
            settings = settings.with_extra_body_value(
                provider.clone(),
                "text",
                json!({"verbosity": verbosity}),
            );
        }
        let tier = self.config.service_tier.clone();
        let tier = if let Some(info) = info {
            info.service_tier_for_request(tier)
        } else {
            tier.filter(|tier| tier == "flex")
        };
        if let Some(tier) = tier {
            settings =
                settings.with_extra_body_value(provider.clone(), "service_tier", json!(tier));
        }
        let metadata = self
            .config
            .responses_api_metadata
            .iter()
            .filter(|(key, _)| forwards_configured_response_metadata(key))
            .collect::<std::collections::BTreeMap<_, _>>();
        if !metadata.is_empty() {
            settings = settings.with_extra_body_value(
                provider.clone(),
                "client_metadata",
                json!(metadata),
            );
        }
        settings
    }

    fn base_instructions(&self, model: Option<&str>) -> Option<String> {
        if let Some(instructions) = &self.config.base_instructions {
            return Some(instructions.clone());
        }
        // An injected SDK resolver owns its model-specific prompt behavior.
        self.registry.as_ref()?;
        let info = model.and_then(|model| self.model_info(model));
        let template = info
            .as_ref()
            .and_then(|info| info.model_messages.as_ref())
            .and_then(|messages| messages.instructions_template.as_deref())
            .unwrap_or(BASE_INSTRUCTIONS);
        Some(if self.config.personality == Some(Personality::None) {
            strip_personality_section(template)
        } else {
            template.to_owned()
        })
    }

    fn compaction_limit(&self, model: &str) -> Option<u64> {
        let info = self.model_info(model);
        let info = info.as_ref();
        let max_window = info
            .and_then(|info| info.max_context_window)
            .or_else(|| info.is_none().then_some(272_000));
        let window = self
            .config
            .model_context_window
            .map(|window| max_window.map_or(window, |max| window.min(max)))
            .or_else(|| info.and_then(ModelInfo::resolved_context_window))
            .or(max_window);
        let limit = self
            .config
            .model_auto_compact_token_limit
            .or_else(|| info.and_then(|info| info.auto_compact_token_limit));
        match (window, limit) {
            (Some(window), limit) => {
                let threshold = window.saturating_mul(9) / 10;
                u64::try_from(limit.map_or(threshold, |limit| limit.min(threshold))).ok()
            }
            (None, limit) => limit.and_then(|limit| u64::try_from(limit).ok()),
        }
    }
}

impl ModelResolver for ProductModelRuntime {
    fn resolve_model(&self, name: Option<&str>) -> Result<ResolvedModel> {
        let name = name.or(self.default_model.as_deref());
        if let Some(explicit) = &self.explicit {
            return explicit.resolve_model(name);
        }
        let registry = self
            .registry
            .as_ref()
            .ok_or_else(|| Error::config("product model provider is unavailable"))?;
        let provider = registry.default_provider();
        let name = name.ok_or_else(|| Error::config("no product model selected"))?;
        // Codex stores provider and model separately. Preserve slashes in a model slug.
        let resolved = registry.resolve_model(Some(&format!("{provider}/{name}")))?;
        Ok(ResolvedModel::new(
            resolved.selector().clone(),
            Arc::clone(resolved.model()),
            resolved.provider_defaults().clone(),
            self.product_settings(name, provider),
        ))
    }
}

impl Capability for ProductModelRuntime {
    fn kind(&self) -> CapabilityFamily {
        CapabilityFamily::COMPACTION
    }

    fn context_processor(&self) -> Option<&dyn ContextProcessor> {
        Some(self)
    }
}

#[async_trait::async_trait]
impl ContextProcessor for ProductModelRuntime {
    async fn process_context(
        &self,
        request: ContextProcessorRequest,
        summarizer: &dyn ContextSummarizer,
    ) -> Result<ContextProcessorResult> {
        let mut compaction = CompactionCapability::default().with_context_windows(
            ContextWindowConfig::default().with_compaction_threshold_ratio(
                ContextWindowThresholdRatio::from_basis_points(9_000),
            ),
        );
        if let Some(model) = request.model_name()
            && (self.explicit.is_none()
                || self.config.model_context_window.is_some()
                || self.config.model_auto_compact_token_limit.is_some()
                || self.config.model_catalog.is_some())
            && let Some(limit) = self.compaction_limit(model)
        {
            let limit = usize::try_from(limit).map_err(|_| {
                Error::config("the model compaction limit does not fit this platform")
            })?;
            compaction = compaction.with_total_token_limit(limit);
        }
        compaction.process_context(request, summarizer).await
    }
}

fn resolve_reasoning_effort(info: Option<&ModelInfo>, effort: &ReasoningEffort) -> ReasoningEffort {
    match effort {
        ReasoningEffort::Ultra => info
            .and_then(|info| {
                info.multi_agent_reasoning_effort
                    .as_ref()
                    .filter(|effort| {
                        **effort != ReasoningEffort::Ultra
                            && info
                                .supported_reasoning_levels
                                .iter()
                                .any(|preset| preset.effort == **effort)
                    })
                    .or_else(|| {
                        info.supported_reasoning_levels
                            .iter()
                            .find(|preset| preset.effort == ReasoningEffort::Max)
                            .or_else(|| {
                                info.supported_reasoning_levels
                                    .iter()
                                    .rev()
                                    .find(|preset| preset.effort != ReasoningEffort::Ultra)
                            })
                            .map(|preset| &preset.effort)
                    })
            })
            .cloned()
            .unwrap_or(ReasoningEffort::Medium),
        ReasoningEffort::Persistent => ReasoningEffort::Custom("disabled".to_owned()),
        effort => effort.clone(),
    }
}

/// Resolves Bedrock's endpoint, request signer, and catalog for one thread.
async fn prepare_bedrock(
    config: &Config,
    bundled: &ModelsResponse,
) -> Result<(
    OpenAiAuth,
    Arc<dyn ResponsesRequestSigner>,
    Arc<ModelsResponse>,
)> {
    let command = config
        .model_provider
        .auth
        .clone()
        .map(CommandBearerToken::new)
        .map(Arc::new);
    let prepared = amazon_bedrock::prepare(config, command).await?;
    let catalog = amazon_bedrock::model_catalog(
        &config.model_provider,
        Some(&prepared.base_url),
        config.model_catalog.as_ref(),
        bundled,
    );
    // Bedrock authenticates through the signer; the endpoint keeps only routing and headers.
    let mut description = config.model_provider.clone();
    description.aws = None;
    description.auth = None;
    description.base_url = Some(prepared.base_url);
    let (auth, _) = provider_auth(&description, false, None)?;
    Ok((auth, prepared.signer, Arc::new(catalog)))
}

fn build_registry(
    config: &Config,
    default_model: String,
    remote: Option<Arc<RemoteModelCatalog>>,
    prepared_auth: Option<(OpenAiAuth, Option<Arc<CommandBearerToken>>)>,
    signer: Option<Arc<dyn ResponsesRequestSigner>>,
) -> Result<ProviderRegistry> {
    let provider = ProviderKey::new(&config.model_provider_id);
    let description = &config.model_provider;
    let protocol = match description.wire_api {
        WireApi::Responses => ApiProtocol::OpenAiResponses,
    };
    let defaults = ModelSettings::new().with_retry(ModelRetrySettings::new().with_max_retries(
        u32::try_from(description.stream_max_retries.unwrap_or(5).min(100)).unwrap_or(100),
    ));
    let description = description.clone();
    let builtin_openai = config.model_provider_id == ra_config::model_provider::OPENAI_PROVIDER_ID;
    let residency = config.enforce_residency.value();
    let http_factory = config.http_client_factory();
    let client = RouteAwareClientPool::new_without_redirects_or_request_logging(
        http_factory.clone(),
        ClientRouteClass::Api,
    )
    .into_client();
    let registration = ProviderRegistration::new(provider.clone(), protocol, move |quirks| {
        let (auth, command) = match &prepared_auth {
            Some(prepared) => prepared.clone(),
            None => (
                provider_auth(&description, builtin_openai, residency)?.0,
                description
                    .auth
                    .clone()
                    .map(CommandBearerToken::new)
                    .map(Arc::new),
            ),
        };
        let idle = Duration::from_millis(description.stream_idle_timeout_ms.unwrap_or(300_000));
        let mut responses = OpenAiResponsesProvider::with_http_client(
            auth,
            &default_model,
            client.clone(),
            idle,
            u32::try_from(description.request_max_retries.unwrap_or(4).min(100)).unwrap_or(100),
        )?
        .with_quirks(quirks);
        if description.supports_websockets {
            let connector =
                ra_http::websocket::WebSocketConnector::new(&http_factory).map_err(|error| {
                    Error::config("failed to initialize Responses WebSocket TLS").with_source(error)
                })?;
            responses = responses.with_websocket(
                connector,
                Duration::from_millis(description.websocket_connect_timeout_ms.unwrap_or(15_000)),
            );
        }
        if let Some(query) = &description.query_params {
            responses = responses.with_default_query(
                query
                    .iter()
                    .map(|(key, value)| (key.clone(), value.as_str().to_owned()))
                    .collect(),
            );
        }
        if let Some(command) = command {
            responses = responses.with_bearer_token(command);
        }
        if let Some(remote) = &remote {
            responses = responses.with_catalog_observer(remote.clone());
        }
        if let Some(signer) = &signer {
            responses = responses.with_request_signer(Arc::clone(signer));
        }
        Ok(Arc::new(responses) as Arc<dyn ModelProvider>)
    })
    .with_defaults(defaults)
    .with_quirks(ProviderQuirks::new().with_prompt_cache_key(true));
    ProviderRegistry::builder(provider)
        .register(registration)
        .build()
}

fn provider_auth(
    description: &ModelProviderInfo,
    builtin_openai: bool,
    residency: Option<ra_config::ResidencyRequirement>,
) -> Result<(OpenAiAuth, Option<String>)> {
    if description.gateway_oauth.is_some() || description.aws.is_some() {
        return Err(Error::config(
            "this model provider requires gateway OAuth or AWS authentication unavailable on the Responses HTTP adapter",
        ));
    }
    let key = match &description.env_key {
        Some(key) => Some(
            std::env::var(key)
                .ok()
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| {
                    let help = description
                        .env_key_instructions
                        .as_deref()
                        .unwrap_or_default();
                    Error::config(format!("missing environment variable `{key}`. {help}"))
                })?,
        ),
        None => description
            .experimental_bearer_token
            .as_ref()
            .map(|token| token.as_str().to_owned())
            .or_else(|| {
                builtin_openai
                    .then(|| std::env::var("OPENAI_API_KEY").ok())
                    .flatten()
                    .filter(|value| !value.trim().is_empty())
            }),
    };
    let base_url = description.base_url.as_deref().unwrap_or(OPENAI_BASE_URL);
    let mut auth = OpenAiAuth::keyless(base_url);
    if let Some(key) = &key {
        auth = auth.with_api_key(key);
    }
    let mut headers = HeaderMap::new();
    for (name, value) in description.http_headers.iter().flatten() {
        if let (Ok(name), Ok(value)) = (
            HeaderName::try_from(name),
            HeaderValue::from_str(value.as_str()),
        ) {
            headers.insert(name, value);
        }
    }
    for (name, key) in description.env_http_headers.iter().flatten() {
        if let Ok(value) = std::env::var(key)
            && !value.trim().is_empty()
            && let (Ok(name), Ok(value)) =
                (HeaderName::try_from(name), HeaderValue::from_str(&value))
        {
            headers.insert(name, value);
        }
    }
    if let Some(ra_config::ResidencyRequirement::Us) = residency {
        headers.insert(RESIDENCY_HEADER_NAME, HeaderValue::from_static("us"));
    }
    for (name, value) in &headers {
        if let Ok(value) = value.to_str() {
            auth = auth.with_default_header(name.as_str(), value);
        }
    }
    Ok((auth, key))
}

fn strip_personality_section(instructions: &str) -> String {
    let mut offset = 0;
    let mut section_start = None;
    for line in instructions.split_inclusive('\n') {
        let heading = line.trim_end_matches(['\r', '\n']);
        if let Some(start) = section_start {
            if heading
                .strip_prefix('#')
                .is_some_and(|rest| rest.is_empty() || rest.starts_with([' ', '\t']))
            {
                return format!("{}{}", &instructions[..start], &instructions[offset..]);
            }
        } else if heading == "# Personality" {
            section_start = Some(offset);
        }
        offset += line.len();
    }
    instructions[..section_start.unwrap_or(instructions.len())].to_owned()
}

impl ContextFilter for ProductModelRuntime {
    fn name(&self) -> &'static str {
        "product_model_context"
    }

    fn filter_model_input(
        &self,
        request: &ContextFilterRequest<'_>,
        data: ModelInputData,
    ) -> Result<ModelInputData> {
        let (mut input, instructions) = data.into_parts();
        if let Some(limit) = self.config.tool_output_token_limit {
            let limit = i64::try_from(limit).unwrap_or(i64::MAX);
            let mode = request
                .model()
                .and_then(|model| self.model_info(model))
                .map_or(TruncationMode::Bytes, |info| info.truncation_policy.mode);
            let policy = match mode {
                TruncationMode::Bytes => TruncationPolicy::bytes(
                    i64::try_from(approx_bytes_for_tokens(limit)).unwrap_or(i64::MAX),
                ),
                TruncationMode::Tokens => TruncationPolicy::tokens(limit),
            };
            for item in &mut input {
                if let ModelInputItem::ToolCallOutput(output) = item
                    && let Some(projected) = truncate_tool_output(output.output(), policy)?
                {
                    *output = output.clone().with_output(projected);
                }
            }
        }
        if let Some(developer) = &self.config.developer_instructions {
            input.insert(0, ModelInputItem::Message(Message::developer(developer)));
        }
        if let Some(base) = self
            .base_instructions(request.model())
            .filter(|base| !base.is_empty())
        {
            let message = match request.session_id() {
                Some(session) => developer_prefix(session, base),
                None => Message::developer(base),
            };
            input.insert(0, ModelInputItem::Message(message));
        }
        Ok(ModelInputData::new(input, instructions))
    }
}

fn truncate_tool_output(
    payload: &serde_json::Value,
    policy: TruncationPolicy,
) -> Result<Option<serde_json::Value>> {
    if let Some(content) = payload.as_str() {
        return Ok(
            (content.len() > policy.byte_budget()).then(|| json!(truncate_text(content, policy)))
        );
    }
    let Some(output) = ToolOutput::from_stored(payload)? else {
        return Ok(None);
    };
    // A hook replacement has already been budgeted using the invoked tool's own policy.
    if output.model_excerpt().is_none() && output.blocks() != output.model_content_blocks() {
        return Ok(None);
    }
    let blocks = output.model_blocks();
    let mut remaining = policy.limit();
    let mut omitted_text = 0;
    let mut projected = Vec::with_capacity(blocks.len());
    for block in &blocks {
        let ToolOutputBlock::Text { text } = block else {
            projected.push(block.clone());
            continue;
        };
        if text.is_empty() {
            continue;
        }
        if remaining == 0 {
            omitted_text += 1;
            continue;
        }
        let (cost, limit) = match policy.mode() {
            ra_core::sandbox::token_truncation::TruncationMode::Tokens => (
                approx_token_count(text),
                TruncationPolicy::tokens(i64::try_from(remaining).unwrap_or(i64::MAX)),
            ),
            _ => (
                text.len(),
                TruncationPolicy::bytes(i64::try_from(remaining).unwrap_or(i64::MAX)),
            ),
        };
        if cost <= remaining {
            projected.push(block.clone());
        } else {
            let snippet = truncate_text(text, limit);
            if snippet.is_empty() {
                omitted_text += 1;
            } else {
                projected.push(ToolOutputBlock::text(snippet));
            }
        }
        remaining = remaining.saturating_sub(cost);
    }
    if omitted_text > 0 {
        projected.push(ToolOutputBlock::text(format!(
            "[omitted {omitted_text} text items ...]"
        )));
    }
    if projected.is_empty() || projected == blocks {
        return Ok(None);
    }
    let output = ToolOutput::new(projected)?;
    serde_json::to_value(output).map(Some).map_err(|error| {
        Error::caller("failed to project model-facing tool output").with_source(error)
    })
}
