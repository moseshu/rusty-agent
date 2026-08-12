//! R1-5b contracts for the explicit wire-protocol capability matrix.

use ra_core::model::{
    ApiProtocol, ReasoningCarrier, ReasoningReplay, ServerConversationSupport,
    StablePrefixLocation, StructuredOutputLocation, ToolCallCarrier, ToolResultCarrier,
};

#[test]
fn test_api_protocol_01() {
    let capabilities = ApiProtocol::OpenAiResponses.capabilities();

    assert_eq!(
        capabilities.reasoning_carrier(),
        ReasoningCarrier::FirstClassItem
    );
    assert_eq!(
        capabilities.reasoning_replay(),
        ReasoningReplay::EncryptedContent
    );
    assert_eq!(
        capabilities.server_conversation(),
        ServerConversationSupport::PreviousResponseIdAndConversationId
    );
    assert_eq!(
        capabilities.stable_prefix(),
        StablePrefixLocation::TopLevelInstructions
    );
    assert!(capabilities.prompt_cache().automatic_prefix_matching());
    assert!(capabilities.prompt_cache().explicit_cache_key());
    assert!(!capabilities.prompt_cache().cache_control_breakpoints());
    assert!(!capabilities.prompt_cache().requires_explicit_breakpoints());
    assert!(capabilities.reasoning_replay().is_required());
    assert_eq!(capabilities.tool_call(), ToolCallCarrier::FunctionCallItem);
    assert_eq!(
        capabilities.tool_result(),
        ToolResultCarrier::FunctionCallOutputItem
    );
    assert_eq!(
        capabilities.structured_output(),
        StructuredOutputLocation::TextFormat
    );
}

#[test]
fn test_api_protocol_02() {
    let capabilities = ApiProtocol::OpenAiChatCompletions.capabilities();

    // reasoning_content is a convention of the Qwen / DeepSeek / Kimi gateways, not a Chat
    // Completions field: first-party OpenAI never returns it, and GPT / Gemini shape their
    // thinking completely differently.
    assert_eq!(capabilities.reasoning_carrier(), ReasoningCarrier::None);
    assert_eq!(capabilities.reasoning_replay(), ReasoningReplay::None);
    assert!(!capabilities.reasoning_replay().is_required());
    assert_eq!(
        capabilities.server_conversation(),
        ServerConversationSupport::None
    );
    assert_eq!(
        capabilities.stable_prefix(),
        StablePrefixLocation::FirstSystemMessage
    );
    // prompt_cache_key exists on Chat too (openai-python chat/completion_create_params.py).
    // Whether a given gateway honors it is a provider fact and belongs to Quirks, not to the
    // protocol matrix.
    assert!(capabilities.prompt_cache().automatic_prefix_matching());
    assert!(capabilities.prompt_cache().explicit_cache_key());
    assert!(!capabilities.prompt_cache().cache_control_breakpoints());
    assert_eq!(capabilities.tool_call(), ToolCallCarrier::MessageToolCalls);
    assert_eq!(
        capabilities.tool_result(),
        ToolResultCarrier::ToolRoleMessage
    );
    assert_eq!(
        capabilities.structured_output(),
        StructuredOutputLocation::ResponseFormat
    );
}

#[test]
fn test_api_protocol_03() {
    let capabilities = ApiProtocol::AnthropicMessages.capabilities();

    assert_eq!(
        capabilities.reasoning_carrier(),
        ReasoningCarrier::ThinkingBlock
    );
    assert_eq!(
        capabilities.reasoning_replay(),
        ReasoningReplay::ThinkingSignature
    );
    assert_eq!(
        capabilities.server_conversation(),
        ServerConversationSupport::None
    );
    assert_eq!(
        capabilities.stable_prefix(),
        StablePrefixLocation::SystemBlockArray
    );
    assert!(!capabilities.prompt_cache().automatic_prefix_matching());
    assert!(!capabilities.prompt_cache().explicit_cache_key());
    assert!(capabilities.prompt_cache().cache_control_breakpoints());
    assert!(
        capabilities.prompt_cache().requires_explicit_breakpoints(),
        "不打断点就完全不缓存"
    );
    assert!(capabilities.reasoning_replay().is_required());
    assert_eq!(capabilities.tool_call(), ToolCallCarrier::ToolUseBlock);
    assert_eq!(
        capabilities.tool_result(),
        ToolResultCarrier::ToolResultBlock
    );
    assert_eq!(
        capabilities.structured_output(),
        StructuredOutputLocation::OutputFormat
    );
}

#[test]
fn test_api_protocol_04() {
    let responses = ApiProtocol::OpenAiResponses.capabilities();
    let chat = ApiProtocol::OpenAiChatCompletions.capabilities();
    let anthropic = ApiProtocol::AnthropicMessages.capabilities();

    assert!(responses.has_first_class_reasoning());
    assert!(responses.supports_server_conversation());
    assert!(responses.supports_previous_response_id());
    assert!(responses.supports_conversation_id());

    assert!(!chat.has_first_class_reasoning());
    assert!(!chat.supports_server_conversation());
    assert!(!chat.supports_previous_response_id());
    assert!(!chat.supports_conversation_id());

    assert!(anthropic.has_first_class_reasoning());
    assert!(!anthropic.supports_server_conversation());
    assert!(!anthropic.supports_previous_response_id());
    assert!(!anthropic.supports_conversation_id());

    // The two OpenAI protocols are isomorphic on caching; separating them would make R1-13 skip
    // prompt_cache_key on Chat. The difference is Anthropic-only: without a breakpoint it does not
    // cache at all.
    assert_eq!(responses.prompt_cache(), chat.prompt_cache());
    assert_ne!(responses.prompt_cache(), anthropic.prompt_cache());
    assert!(responses.prompt_cache().is_supported());
    assert!(chat.prompt_cache().is_supported());
    assert!(anthropic.prompt_cache().is_supported());

    // Chat alone has no reasoning return surface at the protocol level.
    assert!(!chat.reasoning_replay().is_required());
    assert!(responses.reasoning_replay().is_required());
    assert!(anthropic.reasoning_replay().is_required());
}

#[test]
fn test_api_protocol_05() {
    let expected = [
        (ApiProtocol::OpenAiResponses, "openai_responses"),
        (
            ApiProtocol::OpenAiChatCompletions,
            "openai_chat_completions",
        ),
        (ApiProtocol::AnthropicMessages, "anthropic_messages"),
    ];

    assert_eq!(ApiProtocol::ALL.len(), expected.len());

    for ((protocol, name), listed) in expected.into_iter().zip(ApiProtocol::ALL.iter().copied()) {
        assert_eq!(protocol, listed);
        assert_eq!(protocol.as_str(), name);
        assert_eq!(protocol.to_string(), name);

        let encoded = serde_json::to_string(&protocol).expect("协议名应可序列化");
        assert_eq!(encoded, format!("\"{name}\""));
        assert_eq!(
            serde_json::from_str::<ApiProtocol>(&encoded).expect("协议名应可反序列化"),
            protocol
        );
    }
}
