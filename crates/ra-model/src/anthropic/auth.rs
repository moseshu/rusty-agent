//! Authentication and transport defaults for Anthropic Messages.

use std::{collections::BTreeMap, fmt};

use ra_core::error::{Error, Result};

/// Authentication and endpoint configuration for the Anthropic API.
#[non_exhaustive]
#[derive(Clone, PartialEq, Eq)]
pub struct AnthropicAuth {
    api_key: Option<String>,
    base_url: String,
    version: String,
    betas: Vec<String>,
    default_headers: BTreeMap<String, String>,
}

impl AnthropicAuth {
    /// Creates configuration for Anthropic's first-party API.
    #[must_use]
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: Some(api_key.into()),
            base_url: "https://api.anthropic.com/v1".to_owned(),
            version: "2023-06-01".to_owned(),
            betas: Vec::new(),
            default_headers: BTreeMap::new(),
        }
    }

    /// Creates configuration for an endpoint that authenticates outside this adapter.
    #[must_use]
    pub fn keyless(base_url: impl Into<String>) -> Self {
        Self::new(String::new())
            .without_api_key()
            .with_base_url(base_url)
    }

    fn without_api_key(mut self) -> Self {
        self.api_key = None;
        self
    }

    /// Replaces the API base URL. A trailing slash is ignored.
    #[must_use]
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into().trim_end_matches('/').to_owned();
        self
    }
    /// Replaces the API key.
    #[must_use]
    pub fn with_api_key(mut self, api_key: impl Into<String>) -> Self {
        self.api_key = Some(api_key.into());
        self
    }
    /// Selects the protocol version advertised on every request.
    #[must_use]
    pub fn with_version(mut self, version: impl Into<String>) -> Self {
        self.version = version.into();
        self
    }
    /// Adds an Anthropic beta identifier.
    #[must_use]
    pub fn with_beta(mut self, beta: impl Into<String>) -> Self {
        self.betas.push(beta.into());
        self
    }
    /// Adds a non-secret default transport header.
    #[must_use]
    pub fn with_default_header(
        mut self,
        name: impl Into<String>,
        value: impl Into<String>,
    ) -> Self {
        self.default_headers.insert(name.into(), value.into());
        self
    }
    /// Configured base URL without a trailing slash.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }
    /// API version sent in the required transport header.
    #[must_use]
    pub fn version(&self) -> &str {
        &self.version
    }
    /// Configured beta headers.
    #[must_use]
    pub fn betas(&self) -> &[String] {
        &self.betas
    }
    /// Non-secret default headers.
    #[must_use]
    pub const fn default_headers(&self) -> &BTreeMap<String, String> {
        &self.default_headers
    }
    pub(crate) fn api_key(&self) -> Option<&str> {
        self.api_key.as_deref()
    }
    pub(crate) fn validate(&self) -> Result<()> {
        if self
            .api_key
            .as_ref()
            .is_some_and(|key| key.trim().is_empty())
        {
            return Err(Error::config("Anthropic API key must not be empty"));
        }
        if self.base_url.trim().is_empty() {
            return Err(Error::config("Anthropic base URL must not be empty"));
        }
        if self.version.trim().is_empty() {
            return Err(Error::config("Anthropic API version must not be empty"));
        }
        Ok(())
    }
}

impl fmt::Debug for AnthropicAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AnthropicAuth")
            .field("api_key", &self.api_key.as_ref().map(|_| "[REDACTED]"))
            .field("base_url", &self.base_url)
            .field("version", &self.version)
            .field("betas", &self.betas)
            .field("default_header_names", &self.default_headers.keys())
            .finish()
    }
}
