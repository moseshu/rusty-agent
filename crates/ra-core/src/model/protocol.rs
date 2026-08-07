//! Wire-protocol capabilities shared by model adapters.
//!
//! A provider registration is open-ended, but a wire protocol is not. Keeping the protocol set as
//! a closed enum means every protocol must define the complete matrix in this module before an
//! adapter can select it. OpenAI-compatible providers therefore select
//! [`ApiProtocol::OpenAiChatCompletions`] and layer provider quirks on top; compatibility is not a
//! fourth wire protocol.
//!
//! `ModelRequest` remains protocol-neutral. Model adapters lower that request according to this
//! matrix, and the runtime must query capabilities instead of assuming Responses semantics such as
//! first-class reasoning items or `previous_response_id` continuation.
//!
//! # This matrix answers protocol questions only
//!
//! "Does this wire format have a place to put X" belongs here. "Does this particular endpoint
//! accept X" does not — that is a provider fact and lives in `Quirks` (R1-6b) alongside the
//! provider's `extra_body` bucket, so onboarding a vendor stays a one-file change.
//!
//! The distinction is easy to get wrong in both directions:
//!
//! - `prompt_cache_key` is accepted by Responses **and** Chat Completions. Whether a given
//!   endpoint honours it depends on whether it is first-party `OpenAI`, not on which of the two
//!   protocols is in use. The protocol layer records that both mechanisms exist; the provider
//!   layer decides whether to send the key.
//! - `reasoning_content` looks like a Chat Completions field but is not one. Qwen, `DeepSeek` and
//!   Kimi return it; first-party `OpenAI` never does, and GPT and Gemini expose reasoning through
//!   entirely different shapes. Chat Completions therefore carries no reasoning at the protocol
//!   level, and gateways that add it are described by `Quirks`.
//!
//! Both of those were pushed onto `Quirks` by this module, so R1-6b owes two questions its current
//! field list does not cover: **does this endpoint accept `prompt_cache_key`**, and **does this
//! gateway return `reasoning_content`**. Without them the two facts have nowhere to live, and the
//! adapter falls back to guessing from the protocol — which is exactly what this matrix stopped
//! doing.

use std::fmt;

use serde::{Deserialize, Serialize};

/// A wire protocol implemented by a model adapter.
///
/// This is deliberately an enum rather than an extension trait. Providers are extensible through
/// provider registration, while adding a wire protocol requires defining every capability here.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ApiProtocol {
    /// `OpenAI` Responses API.
    #[serde(rename = "openai_responses")]
    OpenAiResponses,
    /// `OpenAI` Chat Completions API.
    #[serde(rename = "openai_chat_completions")]
    OpenAiChatCompletions,
    /// Anthropic Messages API.
    #[serde(rename = "anthropic_messages")]
    AnthropicMessages,
}

impl ApiProtocol {
    /// Protocols with a complete capability matrix.
    ///
    /// A slice rather than an array: `[Self; 3]` would put the protocol count in the public type,
    /// so adding a fourth protocol would break every downstream binding even though the enum
    /// itself is `#[non_exhaustive]`.
    pub const ALL: &'static [Self] = &[
        Self::OpenAiResponses,
        Self::OpenAiChatCompletions,
        Self::AnthropicMessages,
    ];

    /// Stable configuration name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OpenAiResponses => "openai_responses",
            Self::OpenAiChatCompletions => "openai_chat_completions",
            Self::AnthropicMessages => "anthropic_messages",
        }
    }

    /// Complete capability matrix for this protocol.
    #[must_use]
    pub const fn capabilities(self) -> ProtocolCapabilities {
        match self {
            Self::OpenAiResponses => RESPONSES_CAPABILITIES,
            Self::OpenAiChatCompletions => CHAT_COMPLETIONS_CAPABILITIES,
            Self::AnthropicMessages => ANTHROPIC_MESSAGES_CAPABILITIES,
        }
    }
}

impl fmt::Display for ApiProtocol {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// How reasoning is represented on the wire.
///
/// Chat Completions is [`None`](Self::None): the protocol has no response-side reasoning surface
/// at all. Reasoning tokens are billed through `usage.completion_tokens_details`, but no reasoning
/// text comes back. Gateways that do return it — Qwen, `DeepSeek`, Kimi — describe that through
/// `Quirks`, since it is a property of the endpoint rather than of the protocol.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReasoningCarrier {
    /// The protocol returns no reasoning.
    None,
    /// A standalone first-class response item.
    FirstClassItem,
    /// A first-class thinking content block.
    ThinkingBlock,
}

impl ReasoningCarrier {
    /// Whether reasoning has a first-class item or block representation.
    #[must_use]
    pub const fn is_first_class(self) -> bool {
        matches!(self, Self::FirstClassItem | Self::ThinkingBlock)
    }
}

/// Provider material that must be replayed with reasoning.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReasoningReplay {
    /// Nothing has to be replayed, because nothing is returned.
    None,
    /// Responses `encrypted_content`.
    EncryptedContent,
    /// Anthropic thinking-block signature.
    ThinkingSignature,
}

impl ReasoningReplay {
    /// Whether the protocol hands back material that a later turn must resend verbatim.
    #[must_use]
    pub const fn is_required(self) -> bool {
        !matches!(self, Self::None)
    }
}

/// Server-side conversation continuation supported by a protocol.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ServerConversationSupport {
    /// The client must replay all conversation state.
    None,
    /// Both `previous_response_id` and `conversation_id` are available.
    PreviousResponseIdAndConversationId,
}

impl ServerConversationSupport {
    /// Whether any server-side conversation continuation is available.
    #[must_use]
    pub const fn is_supported(self) -> bool {
        !matches!(self, Self::None)
    }

    /// Whether `previous_response_id` continuation is available.
    #[must_use]
    pub const fn supports_previous_response_id(self) -> bool {
        matches!(self, Self::PreviousResponseIdAndConversationId)
    }

    /// Whether `conversation_id` continuation is available.
    #[must_use]
    pub const fn supports_conversation_id(self) -> bool {
        matches!(self, Self::PreviousResponseIdAndConversationId)
    }
}

/// Wire location of the stable prompt prefix.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StablePrefixLocation {
    /// Top-level `instructions`.
    TopLevelInstructions,
    /// The first system message in `messages`.
    FirstSystemMessage,
    /// The top-level `system` block array.
    SystemBlockArray,
}

/// Prompt-cache mechanisms exposed by a protocol.
///
/// A set rather than a single choice, because protocols really do offer more than one at a time:
/// Responses and Chat Completions both cache a stable prefix automatically **and** accept an
/// explicit `prompt_cache_key` to steer routing. Modelling this as one value forces a wrong answer
/// on whichever mechanism loses, and R1-13 would then skip sending the key on a protocol that
/// accepts it.
///
/// Whether a specific endpoint honours the key is a separate, provider-level question for
/// `Quirks`: a third-party gateway speaking Chat Completions may reject it.
///
/// # Known gap: `OpenAI` explicit breakpoints are not represented yet
///
/// Both `OpenAI` protocols now take a request-level `prompt_cache_options` plus a
/// `prompt_cache_breakpoint` on content blocks, on the stable surface — `response_create_params`
/// and `completion_create_params` for the options, `response_input_text_param` and
/// `chat_completion_content_part_text_param` for the breakpoint.
///
/// **Do not express that by setting [`cache_control_breakpoints`](Self::cache_control_breakpoints).**
/// That flag describes Anthropic's mechanism, and the two differ where it matters:
///
/// | | `OpenAI` | Anthropic |
/// | --- | --- | --- |
/// | With no breakpoints | still cached, via an implicit one | nothing is cached |
/// | Turning automatic off | `prompt_cache_options.mode = "explicit"` | not applicable |
/// | TTL | request-level, on the options object | per breakpoint |
///
/// Reusing the Anthropic flag would tell an adapter both that caching requires marks and that TTL
/// travels with each mark. Neither holds for `OpenAI`. A separate capability is the right shape.
///
/// Two things this gap also exposes, worth settling when the capability lands:
///
/// - [`automatic_prefix_matching`](Self::automatic_prefix_matching) is `OpenAI`'s **default mode**,
///   not an invariant: `mode = "explicit"` disables the implicit breakpoint per request. The flag
///   currently reads as though the behaviour were fixed.
/// - The feature is documented as available on `gpt-5.6` and later, so it is gated by **model**,
///   which is a third axis this matrix does not have. Protocol lives here and provider lives in
///   `Quirks`; model capability most plausibly belongs to the resolved-model layer of the R1-2b
///   four-layer settings, decided in R1-3a.
///
/// Deliberately not implemented ahead of a consumer: no adapter exists yet (R1-4 / R1-6), so the
/// shape would be guessed from documentation rather than from a request that actually round-trips.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PromptCacheSupport {
    automatic_prefix_matching: bool,
    explicit_cache_key: bool,
    /// Anthropic `cache_control` only. See the type documentation before reusing this for
    /// `OpenAI`'s `prompt_cache_breakpoint`.
    cache_control_breakpoints: bool,
}

impl PromptCacheSupport {
    /// Whether a stable prompt prefix is cached without any request field.
    #[must_use]
    pub const fn automatic_prefix_matching(self) -> bool {
        self.automatic_prefix_matching
    }

    /// Whether a top-level `prompt_cache_key` steers cache routing.
    #[must_use]
    pub const fn explicit_cache_key(self) -> bool {
        self.explicit_cache_key
    }

    /// Whether `cache_control` breakpoints mark cacheable spans on content blocks.
    #[must_use]
    pub const fn cache_control_breakpoints(self) -> bool {
        self.cache_control_breakpoints
    }

    /// Whether the protocol offers any prompt caching.
    #[must_use]
    pub const fn is_supported(self) -> bool {
        self.automatic_prefix_matching || self.explicit_cache_key || self.cache_control_breakpoints
    }

    /// Whether the caller must place breakpoints for caching to happen at all.
    #[must_use]
    pub const fn requires_explicit_breakpoints(self) -> bool {
        self.cache_control_breakpoints && !self.automatic_prefix_matching
    }
}

/// Wire carrier for a model tool call.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolCallCarrier {
    /// Standalone `function_call` item.
    FunctionCallItem,
    /// An assistant message's `tool_calls` array.
    MessageToolCalls,
    /// A `tool_use` content block.
    ToolUseBlock,
}

/// Wire carrier for a tool result.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolResultCarrier {
    /// Standalone `function_call_output` item.
    FunctionCallOutputItem,
    /// A message with `role: "tool"`.
    ToolRoleMessage,
    /// A `tool_result` content block.
    ToolResultBlock,
}

/// Wire location of structured-output configuration.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StructuredOutputLocation {
    /// Responses `text.format`.
    TextFormat,
    /// Chat Completions `response_format`.
    ResponseFormat,
    /// Anthropic Messages `output_format`.
    OutputFormat,
}

/// Complete, immutable capabilities of one wire protocol.
///
/// Fields are private so consumers cannot construct an incomplete or internally inconsistent
/// matrix. Select a protocol and call [`ApiProtocol::capabilities`] instead.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProtocolCapabilities {
    reasoning_carrier: ReasoningCarrier,
    reasoning_replay: ReasoningReplay,
    server_conversation: ServerConversationSupport,
    stable_prefix: StablePrefixLocation,
    prompt_cache: PromptCacheSupport,
    tool_call: ToolCallCarrier,
    tool_result: ToolResultCarrier,
    structured_output: StructuredOutputLocation,
}

impl ProtocolCapabilities {
    /// Reasoning representation on the wire.
    #[must_use]
    pub const fn reasoning_carrier(self) -> ReasoningCarrier {
        self.reasoning_carrier
    }

    /// Material that must be preserved when replaying reasoning.
    #[must_use]
    pub const fn reasoning_replay(self) -> ReasoningReplay {
        self.reasoning_replay
    }

    /// Server-side conversation-continuation support.
    #[must_use]
    pub const fn server_conversation(self) -> ServerConversationSupport {
        self.server_conversation
    }

    /// Stable prompt-prefix location.
    #[must_use]
    pub const fn stable_prefix(self) -> StablePrefixLocation {
        self.stable_prefix
    }

    /// Prompt-cache mechanisms offered by the protocol.
    #[must_use]
    pub const fn prompt_cache(self) -> PromptCacheSupport {
        self.prompt_cache
    }

    /// Tool-call wire carrier.
    #[must_use]
    pub const fn tool_call(self) -> ToolCallCarrier {
        self.tool_call
    }

    /// Tool-result wire carrier.
    #[must_use]
    pub const fn tool_result(self) -> ToolResultCarrier {
        self.tool_result
    }

    /// Structured-output configuration location.
    #[must_use]
    pub const fn structured_output(self) -> StructuredOutputLocation {
        self.structured_output
    }

    /// Whether reasoning is represented as a first-class item or block.
    #[must_use]
    pub const fn has_first_class_reasoning(self) -> bool {
        self.reasoning_carrier.is_first_class()
    }

    /// Whether any server-side conversation continuation is available.
    #[must_use]
    pub const fn supports_server_conversation(self) -> bool {
        self.server_conversation.is_supported()
    }

    /// Whether `previous_response_id` continuation is available.
    #[must_use]
    pub const fn supports_previous_response_id(self) -> bool {
        self.server_conversation.supports_previous_response_id()
    }

    /// Whether `conversation_id` continuation is available.
    #[must_use]
    pub const fn supports_conversation_id(self) -> bool {
        self.server_conversation.supports_conversation_id()
    }
}

const RESPONSES_CAPABILITIES: ProtocolCapabilities = ProtocolCapabilities {
    reasoning_carrier: ReasoningCarrier::FirstClassItem,
    reasoning_replay: ReasoningReplay::EncryptedContent,
    server_conversation: ServerConversationSupport::PreviousResponseIdAndConversationId,
    stable_prefix: StablePrefixLocation::TopLevelInstructions,
    prompt_cache: PromptCacheSupport {
        automatic_prefix_matching: true,
        explicit_cache_key: true,
        cache_control_breakpoints: false,
    },
    tool_call: ToolCallCarrier::FunctionCallItem,
    tool_result: ToolResultCarrier::FunctionCallOutputItem,
    structured_output: StructuredOutputLocation::TextFormat,
};

const CHAT_COMPLETIONS_CAPABILITIES: ProtocolCapabilities = ProtocolCapabilities {
    // No reasoning at the protocol level: `reasoning_content` is a Qwen / DeepSeek / Kimi
    // gateway convention, not a Chat Completions field, and first-party OpenAI never returns it.
    reasoning_carrier: ReasoningCarrier::None,
    reasoning_replay: ReasoningReplay::None,
    server_conversation: ServerConversationSupport::None,
    stable_prefix: StablePrefixLocation::FirstSystemMessage,
    // Same two mechanisms as Responses; `prompt_cache_key` is accepted here too.
    prompt_cache: PromptCacheSupport {
        automatic_prefix_matching: true,
        explicit_cache_key: true,
        cache_control_breakpoints: false,
    },
    tool_call: ToolCallCarrier::MessageToolCalls,
    tool_result: ToolResultCarrier::ToolRoleMessage,
    structured_output: StructuredOutputLocation::ResponseFormat,
};

const ANTHROPIC_MESSAGES_CAPABILITIES: ProtocolCapabilities = ProtocolCapabilities {
    reasoning_carrier: ReasoningCarrier::ThinkingBlock,
    reasoning_replay: ReasoningReplay::ThinkingSignature,
    server_conversation: ServerConversationSupport::None,
    stable_prefix: StablePrefixLocation::SystemBlockArray,
    // Nothing is cached unless the caller marks it.
    prompt_cache: PromptCacheSupport {
        automatic_prefix_matching: false,
        explicit_cache_key: false,
        cache_control_breakpoints: true,
    },
    tool_call: ToolCallCarrier::ToolUseBlock,
    tool_result: ToolResultCarrier::ToolResultBlock,
    structured_output: StructuredOutputLocation::OutputFormat,
};
