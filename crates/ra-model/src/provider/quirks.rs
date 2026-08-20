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
// This is a capability set, not a state machine: each switch answers an independent question about
// one endpoint, and any combination of answers is a real endpoint somebody has to talk to.
#[allow(clippy::struct_excessive_bools)]
#[non_exhaustive]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct ProviderQuirks {
    prompt_cache_key: bool,
    store: bool,
    stream_usage: bool,
    parallel_tool_calls: bool,
    multimodal_tool_output: bool,
    reasoning_content: bool,
    thinking_blocks: bool,
}

impl ProviderQuirks {
    /// Creates quirks with every capability off.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            prompt_cache_key: false,
            store: false,
            stream_usage: false,
            parallel_tool_calls: false,
            multimodal_tool_output: false,
            reasoning_content: false,
            thinking_blocks: false,
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

    /// Declares that this endpoint accepts a top-level `store` flag.
    ///
    /// First-party `OpenAI` Chat Completions defaults `store` to true so that a completion stays
    /// retrievable, matching Responses. A gateway that does not persist anything has no field to
    /// put it in and rejects the request, so the flag is only sent where retention exists.
    #[must_use]
    pub const fn with_store(mut self, supported: bool) -> Self {
        self.store = supported;
        self
    }

    /// Whether a top-level `store` flag may be sent to this endpoint.
    #[must_use]
    pub const fn store(self) -> bool {
        self.store
    }

    /// Declares that this endpoint accepts `stream_options.include_usage`.
    ///
    /// Without it a streamed call reports no usage at all, so cache-hit rates cannot be computed
    /// for that endpoint. That is still the safer default: many gateways reject the field outright
    /// with an HTTP 400, which costs the whole call rather than one statistic.
    #[must_use]
    pub const fn with_stream_usage(mut self, supported: bool) -> Self {
        self.stream_usage = supported;
        self
    }

    /// Whether `stream_options.include_usage` may be sent to this endpoint.
    #[must_use]
    pub const fn stream_usage(self) -> bool {
        self.stream_usage
    }

    /// Declares that this endpoint accepts `parallel_tool_calls`.
    #[must_use]
    pub const fn with_parallel_tool_calls(mut self, supported: bool) -> Self {
        self.parallel_tool_calls = supported;
        self
    }

    /// Whether `parallel_tool_calls` may be sent to this endpoint.
    #[must_use]
    pub const fn parallel_tool_calls(self) -> bool {
        self.parallel_tool_calls
    }

    /// Declares that this endpoint accepts non-text content in a tool result.
    ///
    /// A `role: "tool"` message carries text on first-party Chat Completions, so an image returned
    /// by a tool is dropped on the way out. Some gateways — Anthropic reached through a translating
    /// proxy, for one — do accept the richer form, and only they should receive it.
    #[must_use]
    pub const fn with_multimodal_tool_output(mut self, supported: bool) -> Self {
        self.multimodal_tool_output = supported;
        self
    }

    /// Whether a tool result may carry non-text content on this endpoint.
    #[must_use]
    pub const fn multimodal_tool_output(self) -> bool {
        self.multimodal_tool_output
    }

    /// Declares that this endpoint speaks `reasoning_content` on Chat Completions messages.
    ///
    /// This is the switch the protocol capability matrix delegates here rather than answering
    /// itself. `reasoning_content` is not a Chat Completions field: first-party `OpenAI` never
    /// returns it, while Qwen, `DeepSeek` and Kimi gateways do. Deriving the answer from the
    /// protocol would be a guess about who is on the other end of the socket.
    #[must_use]
    pub const fn with_reasoning_content(mut self, supported: bool) -> Self {
        self.reasoning_content = supported;
        self
    }

    /// Whether this endpoint produces and accepts `reasoning_content`.
    #[must_use]
    pub const fn reasoning_content(self) -> bool {
        self.reasoning_content
    }

    /// Declares that this endpoint round-trips Anthropic thinking blocks through Chat Completions.
    ///
    /// Interleaved thinking requires the signed blocks to come back verbatim on the next assistant
    /// message. Sending them to an endpoint that does not understand the field is at best ignored
    /// and at worst rejected, and the signatures are useless to a model that did not mint them.
    #[must_use]
    pub const fn with_thinking_blocks(mut self, supported: bool) -> Self {
        self.thinking_blocks = supported;
        self
    }

    /// Whether Anthropic thinking blocks may be replayed to this endpoint.
    #[must_use]
    pub const fn thinking_blocks(self) -> bool {
        self.thinking_blocks
    }
}
