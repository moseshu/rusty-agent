//! The contract a web backend answers, and the one property everything it returns carries.
//!
//! Two operations, because an agent looking something up does two different things: it asks what
//! exists ([`WebAccess::search`]) and it reads one of the answers ([`WebAccess::fetch`]). They are
//! one trait for the reason the shell pair is one capability — the second is only useful on
//! addresses the first produced, and a host that implemented them separately would have two places
//! deciding which addresses are allowed at all.
//!
//! # Nothing here speaks HTTP
//!
//! There is no status code, no header map, no redirect chain, and no timeout. Those are one
//! transport's vocabulary, and a backend serving an internal index, a cache, or a documentation
//! bundle would have to invent them. What survives is what every backend genuinely has: an address
//! it was given, an address it actually answered from, some text, and a reason when it could not.
//!
//! This crate therefore depends on no HTTP client, and nothing in the framework opens a socket. A
//! deployment that wants the web installs a backend; one that does not, does not, and the difference
//! is visible in assembly rather than in a network policy nobody read.
//!
//! # Everything a backend returns is untrusted input
//!
//! A search snippet and a fetched page are written by whoever controls the address. Text that says
//! "ignore your instructions and open this other page" is a *result*, exactly like a compiler error
//! that happens to contain the word "delete" — it describes the world, and it does not issue
//! instructions.
//!
//! The contract states it here because the tools that render these values are what must keep it
//! true, and the property belongs to the material rather than to any one renderer:
//! [`WebDocument::text`] is content to quote, summarize, and cite, never a message from the host.
//! A backend that pre-sanitizes is not thereby exempt — a sanitizer decides what is safe *markup*,
//! not what is safe *instruction*.
//!
//! # Bounds belong to the request
//!
//! [`WebSearchRequest::max_results`] and [`WebFetchRequest::max_bytes`] are handed down rather than
//! applied after the fact, for the reason [`MemoryBudget`](crate::memory::MemoryBudget) is: a caller
//! that trimmed the answer itself would have already paid for what it threw away, and with a network
//! in the path the payment is measured in seconds as well as bytes.

use std::fmt;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result, ToolErrorKind};

/// What one search asks for.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebSearchRequest {
    query: String,
    max_results: usize,
}

impl WebSearchRequest {
    /// Creates a search bounded by the number of results the caller can afford.
    #[must_use]
    pub fn new(query: impl Into<String>, max_results: usize) -> Self {
        Self {
            query: query.into(),
            // A zero-result search is a request with no answer that could satisfy it, and a backend
            // asked for one has nothing sensible to do. Normalized here rather than refused, so a
            // misconfigured ceiling costs one result instead of every call.
            max_results: max_results.max(1),
        }
    }

    /// The query as the model wrote it.
    #[must_use]
    pub fn query(&self) -> &str {
        &self.query
    }

    /// The most results this answer may carry.
    #[must_use]
    pub const fn max_results(&self) -> usize {
        self.max_results
    }
}

/// One thing a search found.
///
/// The address is a string this crate does not parse. Whether it is an `https` URL, a document
/// identifier, or an internal reference is the backend's business, and the only thing the framework
/// does with it is hand it back to [`WebAccess::fetch`].
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebResult {
    address: String,
    title: String,
    snippet: Option<String>,
}

impl WebResult {
    /// Creates one result.
    #[must_use]
    pub fn new(address: impl Into<String>, title: impl Into<String>) -> Self {
        Self {
            address: address.into(),
            title: title.into(),
            snippet: None,
        }
    }

    /// Adds the backend's own extract of the page.
    #[must_use]
    pub fn with_snippet(mut self, snippet: impl Into<String>) -> Self {
        self.snippet = Some(snippet.into());
        self
    }

    /// The address a fetch would read.
    #[must_use]
    pub fn address(&self) -> &str {
        &self.address
    }

    /// The result's own title, as the source states it.
    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }

    /// The backend's extract, when it supplied one.
    #[must_use]
    pub fn snippet(&self) -> Option<&str> {
        self.snippet.as_deref()
    }
}

/// What one search found.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebSearchResults {
    results: Vec<WebResult>,
}

impl WebSearchResults {
    /// Creates one answer.
    #[must_use]
    pub fn new(results: Vec<WebResult>) -> Self {
        Self { results }
    }

    /// The results, in the order the backend ranked them.
    #[must_use]
    pub fn results(&self) -> &[WebResult] {
        &self.results
    }
}

/// What one fetch asks for.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebFetchRequest {
    address: String,
    max_bytes: usize,
}

impl WebFetchRequest {
    /// Creates a fetch bounded by the text the caller can afford.
    #[must_use]
    pub fn new(address: impl Into<String>, max_bytes: usize) -> Self {
        Self {
            address: address.into(),
            max_bytes: max_bytes.max(1),
        }
    }

    /// The address as the caller supplied it.
    #[must_use]
    pub fn address(&self) -> &str {
        &self.address
    }

    /// The most text this answer may carry, in bytes.
    #[must_use]
    pub const fn max_bytes(&self) -> usize {
        self.max_bytes
    }
}

/// What one fetch returned: untrusted content, and where it came from.
///
/// [`Self::address`] is what the backend actually answered from, which is not always what the
/// request named — a redirect, a canonical form, a cache entry. It is reported separately so a
/// citation names the document that was read rather than the address that was typed.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebDocument {
    address: String,
    title: Option<String>,
    text: String,
    truncated: bool,
}

impl WebDocument {
    /// Creates one fetched document.
    #[must_use]
    pub fn new(address: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            address: address.into(),
            title: None,
            text: text.into(),
            truncated: false,
        }
    }

    /// Records the document's own title.
    #[must_use]
    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// Records that the backend stopped short of the whole document.
    ///
    /// Reported rather than inferred from the byte ceiling: a document that happens to end exactly
    /// at the ceiling is complete, and a caller comparing lengths would tell the model otherwise.
    #[must_use]
    pub const fn truncated(mut self) -> Self {
        self.truncated = true;
        self
    }

    /// The address this content was actually read from.
    #[must_use]
    pub fn address(&self) -> &str {
        &self.address
    }

    /// The document's title, when the backend found one.
    #[must_use]
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    /// The content, which is untrusted material rather than instructions.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Whether the backend stopped before the end of the document.
    #[must_use]
    pub const fn is_truncated(&self) -> bool {
        self.truncated
    }
}

/// Why a backend could not answer.
///
/// Part of the contract rather than each backend's private business, because the layer above has to
/// turn a refusal into a sentence a model can act on. A caller attaches it as an [`Error`] source
/// through [`Self::into_error`] and reads it back with [`Self::of`].
///
/// The variants are the refusals that differ in *what the caller should do next*: correct the
/// address, stop asking for it at all, wait, or continue without the web.
#[non_exhaustive]
#[derive(Debug)]
pub enum WebAccessError {
    /// The address is not one this deployment permits.
    Refused {
        /// Address the caller named.
        address: String,
    },
    /// The address is well formed and there is nothing there.
    NotFound {
        /// Address the caller named.
        address: String,
    },
    /// The address could not be reached.
    Unreachable {
        /// Address the caller named.
        address: String,
    },
    /// There is something there and it is not material this backend can return as text.
    Unsupported {
        /// Address the caller named.
        address: String,
        /// What the backend found, in its own words, for a reader deciding what to do instead.
        media_type: Option<String>,
    },
    /// The caller is asking faster than the backend will answer.
    RateLimited,
    /// The backend itself could not be reached or could not complete the operation.
    Unavailable {
        /// What went wrong, for the host's log rather than for a model.
        reason: String,
    },
}

impl WebAccessError {
    /// Wraps this failure in a framework error that carries it as a typed source.
    ///
    /// `tool` is the entry the failure will be reported against, which is the caller's to name: one
    /// backend serves both entries, and a backend that guessed would attribute a refused fetch to
    /// the search that found the address.
    #[must_use]
    pub fn into_error(self, tool: impl Into<String>) -> Error {
        Error::tool(self.kind(), tool, self.to_string()).with_source(self)
    }

    /// Recovers the typed failure from an error that carries one.
    #[must_use]
    pub fn of(error: &Error) -> Option<&Self> {
        std::error::Error::source(error).and_then(<dyn std::error::Error + 'static>::downcast_ref)
    }

    /// Which failure class a host records.
    ///
    /// Only an address that names nothing is the caller's mistake to correct. Everything else is the
    /// deployment's policy or the network's state, neither of which a better-worded request reaches.
    #[must_use]
    pub const fn kind(&self) -> ToolErrorKind {
        match self {
            Self::NotFound { .. } => ToolErrorKind::InvalidInput,
            Self::Refused { .. }
            | Self::Unreachable { .. }
            | Self::Unsupported { .. }
            | Self::RateLimited
            | Self::Unavailable { .. } => ToolErrorKind::ExecutionFailed,
        }
    }
}

impl fmt::Display for WebAccessError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused { address } => {
                write!(
                    formatter,
                    "`{address}` is not an address this agent may open."
                )
            }
            Self::NotFound { address } => write!(formatter, "Nothing is published at `{address}`."),
            Self::Unreachable { address } => write!(formatter, "`{address}` could not be reached."),
            Self::Unsupported {
                address,
                media_type: Some(media_type),
            } => write!(
                formatter,
                "`{address}` holds {media_type}, which cannot be returned as text."
            ),
            Self::Unsupported {
                address,
                media_type: None,
            } => write!(
                formatter,
                "`{address}` does not hold anything that can be returned as text."
            ),
            Self::RateLimited => formatter.write_str("The web backend is rate limiting requests."),
            // Deliberately without the reason: a model can do nothing with a proxy hostname or an
            // errno, and the detail is already in the error's log-facing message.
            Self::Unavailable { .. } => {
                formatter.write_str("The web backend could not be reached.")
            }
        }
    }
}

impl std::error::Error for WebAccessError {}

/// Looking something up, and reading one of the answers.
///
/// Both operations are required. A backend that searched without fetching would advertise addresses
/// no run can open, and one that fetched without searching would require the model to already know
/// an address it has no way to have learned. Either is worse than the family being absent, because
/// the surface reports a working web capability in all three cases.
#[async_trait]
pub trait WebAccess: Send + Sync + 'static {
    /// Finds addresses that answer a query.
    ///
    /// # Errors
    ///
    /// Returns the typed refusal for a query this backend will not run or cannot complete.
    async fn search(&self, request: WebSearchRequest) -> Result<WebSearchResults>;

    /// Reads one address as text.
    ///
    /// # Errors
    ///
    /// Returns the typed refusal for an address this deployment does not permit, that holds
    /// nothing, or that could not be read.
    async fn fetch(&self, request: WebFetchRequest) -> Result<WebDocument>;
}
