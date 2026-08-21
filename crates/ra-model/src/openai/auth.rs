//! `OpenAI` API authentication and endpoint configuration.

use std::{collections::BTreeMap, fmt};

/// Authentication and transport defaults shared by `OpenAI` protocol adapters.
///
/// The API key and header values are intentionally omitted from `Debug`. Per-request headers from
/// `ModelSettings` are applied after these defaults, except that the provider always owns the
/// `Authorization` header.
#[non_exhaustive]
#[derive(Clone, PartialEq, Eq)]
pub struct OpenAiAuth {
    api_key: Option<String>,
    base_url: String,
    organization: Option<String>,
    project: Option<String>,
    default_headers: BTreeMap<String, String>,
}

impl OpenAiAuth {
    /// Creates configuration for the official `https://api.openai.com/v1` endpoint.
    #[must_use]
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: Some(api_key.into()),
            base_url: "https://api.openai.com/v1".to_owned(),
            organization: None,
            project: None,
            default_headers: BTreeMap::new(),
        }
    }

    /// Creates configuration for an endpoint that authenticates nothing.
    ///
    /// Model servers run on the developer's own machine — Ollama, vLLM, llama.cpp, LM Studio —
    /// speak this protocol without any credential. The alternative is to invent a placeholder
    /// secret to get past validation, which puts a value that means nothing into configuration
    /// files and logs, and hides the one case where a missing key really is a mistake.
    ///
    /// The base URL is required here rather than defaulted: an unauthenticated request to the
    /// first-party endpoint has no meaning.
    #[must_use]
    pub fn keyless(base_url: impl Into<String>) -> Self {
        Self {
            api_key: None,
            base_url: String::new(),
            organization: None,
            project: None,
            default_headers: BTreeMap::new(),
        }
        .with_base_url(base_url)
    }

    /// Replaces the API base URL. A trailing slash is ignored.
    #[must_use]
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        let base_url = base_url.into();
        base_url
            .trim_end_matches('/')
            .clone_into(&mut self.base_url);
        self
    }

    /// Sets the bearer credential, replacing whatever was configured before.
    #[must_use]
    pub fn with_api_key(mut self, api_key: impl Into<String>) -> Self {
        self.api_key = Some(api_key.into());
        self
    }

    /// Selects an `OpenAI` organization.
    #[must_use]
    pub fn with_organization(mut self, organization: impl Into<String>) -> Self {
        self.organization = Some(organization.into());
        self
    }

    /// Selects an `OpenAI` project.
    #[must_use]
    pub fn with_project(mut self, project: impl Into<String>) -> Self {
        self.project = Some(project.into());
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

    /// Optional organization identifier.
    #[must_use]
    pub fn organization(&self) -> Option<&str> {
        self.organization.as_deref()
    }

    /// Optional project identifier.
    #[must_use]
    pub fn project(&self) -> Option<&str> {
        self.project.as_deref()
    }

    /// Non-secret default headers. Values are still redacted from `Debug`.
    #[must_use]
    pub const fn default_headers(&self) -> &BTreeMap<String, String> {
        &self.default_headers
    }

    /// The bearer credential, when this endpoint takes one.
    pub(crate) fn api_key(&self) -> Option<&str> {
        self.api_key.as_deref()
    }

    pub(crate) fn validate(&self) -> ra_core::error::Result<()> {
        // A blank string is still rejected. `keyless` is how an endpoint declares it wants no
        // credential; an empty one reaching here means a key was configured from something that
        // turned out to be unset, and sending the request unauthenticated would report it as a
        // rejection from the endpoint instead of as the local misconfiguration it is.
        if self
            .api_key
            .as_ref()
            .is_some_and(|key| key.trim().is_empty())
        {
            return Err(ra_core::error::Error::config(
                "OpenAI API key must not be empty",
            ));
        }
        if self.base_url.trim().is_empty() {
            return Err(ra_core::error::Error::config(
                "OpenAI base URL must not be empty",
            ));
        }
        Ok(())
    }
}

impl fmt::Debug for OpenAiAuth {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenAiAuth")
            // Whether a credential exists is configuration worth seeing; its value never is.
            .field("api_key", &self.api_key.as_ref().map(|_| "[REDACTED]"))
            .field("base_url", &self.base_url)
            .field("organization", &self.organization)
            .field("project", &self.project)
            .field("default_header_names", &self.default_headers.keys())
            .finish()
    }
}
