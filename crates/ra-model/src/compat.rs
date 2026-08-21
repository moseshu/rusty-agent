//! Third-party `OpenAI`-compatible endpoints: the Chat protocol implementation plus difference switches.
//!
//! A compatible endpoint is not a fourth protocol. A relay, a router, or a model server on the
//! developer's own machine speaks Chat Completions byte for byte; what differs is which optional
//! request fields it accepts, and how its stream has to be read. So this layer contributes no
//! codec of its own — it configures the Chat adapter, which is where every one of those decisions
//! already had a switch.
//!
//! **Nothing optional is sent until it is declared.** The conservative direction is not caution
//! for its own sake: `stream_options.include_usage` sent to a gateway that does not implement it
//! comes back as an HTTP 400, so a first-party default costs the whole call rather than the one
//! statistic it was reaching for. Starting from "send nothing unusual" makes onboarding a sequence
//! of confirmations instead of a sequence of rejected requests.
//!
//! **One endpoint is one value.** Base URL, credential (or the declared absence of one), the
//! capability switches, the codec quirks, and the non-standard body fields the vendor requires all
//! live on [`CompatEndpoint`], which becomes a provider registration. Capability switches and
//! `extra_body` are two halves of one question — which standard fields this endpoint cannot
//! accept, and which non-standard ones it additionally needs — and answering them in two places is
//! how the two descriptions of one endpoint start to disagree.

pub mod quirks;

use std::{fmt, sync::Arc};

use ra_core::{
    error::{Error, Result},
    model::{ApiProtocol, JsonMap, ModelProvider, ProviderKey},
};

use self::quirks::DoneMarker;
use crate::{
    openai::{
        auth::OpenAiAuth,
        chat::{ChatLoweringOptions, OpenAiChatProvider, reasoning::ReasoningReplayPolicy},
    },
    provider::{ProviderRegistration, quirks::ProviderQuirks},
};

/// The request path the adapter appends, which a base URL must therefore not already contain.
const REQUEST_PATH: &str = "/chat/completions";

/// One `OpenAI`-compatible endpoint and everything currently known about it.
#[non_exhaustive]
#[derive(Clone)]
pub struct CompatEndpoint {
    auth: OpenAiAuth,
    default_model: String,
    quirks: ProviderQuirks,
    done_marker: DoneMarker,
    options: ChatLoweringOptions,
    replay: ReasoningReplayPolicy,
    extra_body: JsonMap,
}

impl CompatEndpoint {
    /// Describes an endpoint that requires no credential, such as a local model server.
    ///
    /// `base_url` is the root the adapter appends `/chat/completions` to. Add a credential with
    /// [`Self::with_api_key`]; relays generally require one and local servers generally do not.
    #[must_use]
    pub fn new(base_url: impl Into<String>, default_model: impl Into<String>) -> Self {
        Self {
            auth: OpenAiAuth::keyless(base_url),
            default_model: default_model.into(),
            quirks: ProviderQuirks::new(),
            done_marker: DoneMarker::Standard,
            options: ChatLoweringOptions::new(),
            replay: ReasoningReplayPolicy::default(),
            extra_body: JsonMap::new(),
        }
    }

    /// Sets the bearer credential this endpoint authenticates with.
    #[must_use]
    pub fn with_api_key(mut self, api_key: impl Into<String>) -> Self {
        self.auth = self.auth.with_api_key(api_key);
        self
    }

    /// Adds a non-secret default transport header.
    ///
    /// Some routers read attribution or routing headers that are part of the endpoint rather than
    /// of any one request. The adapter still owns `Authorization`, which is set from the key.
    #[must_use]
    pub fn with_default_header(
        mut self,
        name: impl Into<String>,
        value: impl Into<String>,
    ) -> Self {
        self.auth = self.auth.with_default_header(name, value);
        self
    }

    /// Declares which optional request fields this endpoint accepts.
    #[must_use]
    pub const fn with_quirks(mut self, quirks: ProviderQuirks) -> Self {
        self.quirks = quirks;
        self
    }

    /// Declares how this endpoint terminates a streamed response.
    #[must_use]
    pub fn with_done_marker(mut self, done_marker: DoneMarker) -> Self {
        self.done_marker = done_marker;
        self
    }

    /// Sets the caller policy for features Chat Completions cannot express.
    ///
    /// The default refuses to degrade silently, and it is worth keeping through a relay: an
    /// endpoint reached this way is the one most likely to be missing something, and a dropped
    /// feature surfaces later as a model that ignored an instruction nobody can find.
    #[must_use]
    pub const fn with_lowering_options(mut self, options: ChatLoweringOptions) -> Self {
        self.options = options;
        self
    }

    /// Replaces the per-item reasoning replay policy.
    #[must_use]
    pub fn with_reasoning_replay(mut self, replay: ReasoningReplayPolicy) -> Self {
        self.replay = replay;
        self
    }

    /// Sets the non-standard request-body fields this endpoint requires.
    ///
    /// This is the half of endpoint configuration that capability switches cannot express: vLLM's
    /// `guided_json`, a router's `provider` or `transforms` block, a vendor's own sampling knob.
    /// They are the base the adapter builds its request on top of, and they reach a request
    /// through the settings layer of a provider registration — see [`Self::build_provider`] for
    /// what that means for the entry point that builds no registration.
    #[must_use]
    pub fn with_extra_body(mut self, extra_body: JsonMap) -> Self {
        self.extra_body = extra_body;
        self
    }

    /// Endpoint root, without a trailing slash.
    #[must_use]
    pub fn base_url(&self) -> &str {
        self.auth.base_url()
    }

    /// Model used when a caller names none.
    #[must_use]
    pub fn default_model(&self) -> &str {
        &self.default_model
    }

    /// Declared request-field capabilities.
    #[must_use]
    pub const fn quirks(&self) -> ProviderQuirks {
        self.quirks
    }

    /// Declared stream terminator.
    #[must_use]
    pub const fn done_marker(&self) -> &DoneMarker {
        &self.done_marker
    }

    /// Declared non-standard request-body fields.
    #[must_use]
    pub const fn extra_body(&self) -> &JsonMap {
        &self.extra_body
    }

    /// Builds a provider for this endpoint without going through a registry.
    ///
    /// An endpoint with static body fields is refused here rather than served without them. Those
    /// fields travel on the settings layer of a provider registration, which this entry point does
    /// not build, so honouring the declaration is not something it can do — and a vendor that
    /// requires a field generally requires it to answer at all, which would surface as the
    /// endpoint rejecting every request rather than as the configuration having been dropped.
    pub fn build_provider(&self) -> Result<OpenAiChatProvider> {
        if !self.extra_body.is_empty() {
            return Err(Error::caller(
                "compat extra_body reaches a request through a provider registration's settings \
                 layer; register this endpoint with `into_registration` instead of building its \
                 provider directly, or drop the static body fields",
            ));
        }
        self.provider_with(self.quirks)
    }

    /// Turns this endpoint into a provider registration under `key`.
    ///
    /// The registration carries the capability switches and the static body fields, so the
    /// registry stays the one place that describes the endpoint; a caller that overrides either on
    /// the registration afterwards wins, because the factory uses what the registry hands it
    /// rather than the copy captured here.
    ///
    /// Configuration is validated now rather than at the first resolution. The registry builds
    /// providers lazily, and a base URL that is wrong would otherwise be discovered by a run
    /// already in progress instead of by the process that read the configuration.
    pub fn into_registration(self, key: ProviderKey) -> Result<ProviderRegistration> {
        self.validate()?;
        let declared = self.quirks;
        let extra_body = self.extra_body.clone();
        let endpoint = Arc::new(self);

        let mut registration = ProviderRegistration::new(
            key,
            ApiProtocol::OpenAiChatCompletions,
            move |quirks: ProviderQuirks| -> Result<Arc<dyn ModelProvider>> {
                endpoint
                    .provider_with(quirks)
                    .map(|provider| Arc::new(provider) as Arc<dyn ModelProvider>)
            },
        )
        .with_quirks(declared);
        if !extra_body.is_empty() {
            registration = registration.with_static_extra_body(extra_body);
        }
        Ok(registration)
    }

    fn provider_with(&self, quirks: ProviderQuirks) -> Result<OpenAiChatProvider> {
        self.validate()?;
        Ok(
            OpenAiChatProvider::new(self.auth.clone(), self.default_model.clone())?
                .with_quirks(quirks)
                .with_lowering_options(self.options)
                .with_reasoning_replay(self.replay.clone())
                .with_terminator(self.done_marker.terminator()),
        )
    }

    /// Checks the parts of this configuration that can be wrong locally.
    ///
    /// The base URL check earns its place: pasting the full endpoint URL out of a vendor's
    /// documentation is the most common way to misconfigure a relay, and left alone it produces a
    /// 404 from a URL nobody printed — which reads as the endpoint being down rather than as the
    /// base URL naming the request path twice.
    fn validate(&self) -> Result<()> {
        self.auth.validate()?;
        // A frame payload is compared to this marker after trimming, which makes both of these
        // configurations unusable in opposite ways.
        if let DoneMarker::Literal(marker) = &self.done_marker {
            if marker.trim().is_empty() {
                return Err(Error::config(
                    "compat stream done marker must not be empty or whitespace",
                ));
            }
            if marker != marker.trim() {
                return Err(Error::config(format!(
                    "compat stream done marker `{marker}` is padded with whitespace, so it can \
                     never match a trimmed frame payload; the stream would end by failing to parse \
                     that frame as JSON instead of by recognizing it"
                )));
            }
        }
        if self.base_url().ends_with(REQUEST_PATH) {
            return Err(Error::config(format!(
                "compat base URL `{}` already ends in `{REQUEST_PATH}`, which the adapter appends \
                 itself; configure the endpoint root instead",
                self.base_url()
            )));
        }
        if self.default_model.trim().is_empty() {
            return Err(Error::config("compat default model must not be empty"));
        }
        Ok(())
    }
}

impl fmt::Debug for CompatEndpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CompatEndpoint")
            .field("auth", &self.auth)
            .field("default_model", &self.default_model)
            .field("quirks", &self.quirks)
            .field("done_marker", &self.done_marker)
            .field("options", &self.options)
            // A vendor's required body fields can carry a credential of their own. Their names are
            // configuration worth seeing; their values follow the same rule as the API key.
            .field("extra_body_keys", &self.extra_body.keys())
            .finish_non_exhaustive()
    }
}
