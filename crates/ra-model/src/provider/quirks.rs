//! Endpoint capabilities that the wire protocol cannot answer.
//!
//! These are **provider** facts, not protocol facts, and the distinction is the reason this module
//! exists. Whether an endpoint accepts a given request field follows from which vendor answers it,
//! not from which wire format the request is written in: a compatible gateway behind a custom base
//! URL speaks byte-identical Responses or Chat Completions and may still reject a first-party-only
//! field. Deriving the answer from the protocol capability matrix would restore exactly the guess
//! that matrix exists to remove.
//!
//! Every switch here is **opt-in**. The default is the behaviour that works against any endpoint
//! speaking the protocol, so onboarding a gateway starts from "send nothing unusual" and adds
//! capabilities as they are confirmed, rather than starting from first-party assumptions and
//! discovering the differences through rejected requests.

/// Endpoint capability switches declared on a provider registration.
///
/// Declaring them here keeps everything known about one endpoint — credentials, base URL, aliases,
/// static body fields, and these switches — configured in a single place.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct ProviderQuirks {
    prompt_cache_key: bool,
}

impl ProviderQuirks {
    /// Creates quirks with every capability off.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            prompt_cache_key: false,
        }
    }

    /// Declares that this endpoint honours a top-level `prompt_cache_key`.
    ///
    /// Enable it for first-party `OpenAI`. A gateway that merely speaks the same protocol may
    /// reject the field outright, and one that ignores it is no better off for receiving it, so a
    /// cache scope is sent only where it is known to mean something.
    #[must_use]
    pub const fn with_prompt_cache_key(mut self, supported: bool) -> Self {
        self.prompt_cache_key = supported;
        self
    }

    /// Whether a top-level `prompt_cache_key` may be sent to this endpoint.
    #[must_use]
    pub const fn prompt_cache_key(self) -> bool {
        self.prompt_cache_key
    }
}
