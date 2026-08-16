//! Smoke tests against a real endpoint. `#[ignore]` by default, so offline CI never reaches them.
//!
//! Run with:
//! ```text
//! RA_LIVE_OPENAI_BASE_URL=https://api.bianxie.ai/v1 \
//! RA_LIVE_OPENAI_API_KEY="$OPENAI_API_KEY1" \
//! RA_LIVE_OPENAI_MODEL=gpt-5.5 \
//! cargo test --manifest-path tests/Cargo.toml -p it-model --test openai_responses_live -- --ignored --nocapture
//! ```
//!
//! The value of these cases is not in their assertions but in the **measured facts they print**:
//! whether a third-party endpoint honors `include: reasoning.encrypted_content`, whether reasoning
//! replays verbatim, and whether `cached_tokens` and `x-request-id` survive the hop. The findings
//! belong back in the `Quirks` of R1-6b and the usage detail of R1-8.

use ra_core::{
    item::{ModelInputItem, RunItemKind, ToolCallOutput},
    model::{Effort, Model, ModelRequest, ModelSettings, ModelToolDefinition, ProviderKey},
};
use ra_model::openai::{auth::OpenAiAuth, responses::OpenAiResponsesModel};
use serde_json::json;

fn required_env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| {
        panic!("live test requires env var {name}; see the command at the top of this file")
    })
}

fn live_model() -> OpenAiResponsesModel {
    OpenAiResponsesModel::new(
        required_env("RA_LIVE_OPENAI_MODEL"),
        OpenAiAuth::new(required_env("RA_LIVE_OPENAI_API_KEY"))
            .with_base_url(required_env("RA_LIVE_OPENAI_BASE_URL")),
    )
    .expect("the live model should build")
}

fn resolved(settings: ModelSettings) -> ra_core::model::ResolvedModelSettings {
    ModelSettings::new().resolve(
        &ProviderKey::new("openai"),
        &ModelSettings::new(),
        &ModelSettings::new(),
        &settings,
    )
}

fn report(label: &str, response: &ra_core::item::ModelResponse) {
    eprintln!("--- {label} ---");
    eprintln!(
        "response_id={:?} request_id={:?}",
        response.response_id(),
        response.request_id()
    );
    eprintln!(
        "usage: input={} output={} cached={} reasoning={}",
        response.usage().input_tokens(),
        response.usage().output_tokens(),
        response.usage().cached_input_tokens(),
        response.usage().reasoning_tokens()
    );
    for item in response.output() {
        match item.kind() {
            RunItemKind::Reasoning(reasoning) => eprintln!(
                "  reasoning id={:?} encrypted={} summary={} content={}",
                reasoning.id(),
                reasoning.encrypted_content().is_some(),
                reasoning.summary().len(),
                reasoning.content().len()
            ),
            RunItemKind::Message(message) => eprintln!(
                "  message phase={:?} blocks={}",
                message.phase(),
                message.content().len()
            ),
            RunItemKind::ToolCall(call) => {
                eprintln!("  tool_call {} args={}", call.name(), call.arguments());
            }
            other => eprintln!("  {}", other.label()),
        }
    }
}

/// One minimal round trip: whether a real server accepts the request shape.
#[tokio::test]
#[ignore = "hits a real endpoint and needs credentials"]
async fn live_minimal_turn_round_trips() {
    let model = live_model();
    let response = model
        .get_response(
            ModelRequest::new(
                vec![ModelInputItem::Message(ra_core::item::Message::user(
                    "Reply with exactly: pong",
                ))],
                resolved(ModelSettings::new().with_max_tokens(2048)),
            )
            .with_system_instructions("You are a terse test fixture."),
        )
        .await
        .expect("the minimal round trip should succeed");

    report("minimal round trip", &response);
    assert!(
        response.response_id().is_some(),
        "the server should return a response id"
    );
    assert!(
        response.usage().input_tokens() > 0,
        "usage should report input tokens"
    );
}

/// Two turns: replays the first turn's reasoning and tool call verbatim to exercise the
/// `encrypted_content` path.
///
/// This is the part of R1-4 most likely to break against a real endpoint: with `store=false`,
/// reasoning depends on `include: reasoning.encrypted_content` for its replay material, and a
/// relay may well swallow it.
#[tokio::test]
#[ignore = "hits a real endpoint and needs credentials"]
async fn live_reasoning_and_tool_call_replay() {
    let model = live_model();
    let tools = vec![
        ModelToolDefinition::new(
            "get_weather",
            json!({
                "type": "object",
                "properties": {"city": {"type": "string"}},
                "required": ["city"],
                "additionalProperties": false
            }),
        )
        .with_description("Return the current weather for a city."),
    ];

    // High effort plus a question that requires deduction: only then does the first turn produce
    // both reasoning and a tool call, which is what actually exercises the replay path. A simple
    // question makes the model call the tool directly with zero reasoning, testing nothing.
    let settings = || {
        resolved(
            ModelSettings::new()
                .with_max_tokens(4096)
                .with_effort(Effort::High),
        )
    };
    let first = model
        .get_response(
            ModelRequest::new(
                vec![ModelInputItem::Message(ra_core::item::Message::user(
                    "I land tomorrow in the Chinese city that hosted the 2022 Asian Games and \
                     is the headquarters of Alibaba. Work out which city that is, then call the \
                     weather tool exactly once for it.",
                ))],
                settings(),
            )
            .with_tools(tools.clone()),
        )
        .await
        .expect("the first turn should succeed");
    report("first turn (expecting reasoning plus a tool call)", &first);

    let call_id = first
        .output()
        .iter()
        .find_map(|item| match item.kind() {
            RunItemKind::ToolCall(call) => Some(call.call_id().clone()),
            _ => None,
        })
        .expect("the first turn should produce one tool call");
    // Whether the model emits reasoning this turn is its own call and is not asserted; but if it
    // does, the replay material has to come with it. Missing material means the relay swallowed
    // `include: reasoning.encrypted_content`, which is an R1-6b quirk.
    let reasoning_items = first
        .output()
        .iter()
        .filter_map(|item| match item.kind() {
            RunItemKind::Reasoning(reasoning) => Some(reasoning),
            _ => None,
        })
        .collect::<Vec<_>>();
    eprintln!("reasoning items = {}", reasoning_items.len());
    assert!(
        reasoning_items
            .iter()
            .all(|reasoning| reasoning.encrypted_content().is_some()),
        "the endpoint returned reasoning without encrypted_content: it cannot be replayed under store=false"
    );

    let mut input = first.to_input_items();
    input.push(ModelInputItem::ToolCallOutput(ToolCallOutput::new(
        call_id,
        json!({"temp_c": 21, "condition": "clear"}),
    )));

    let second = model
        .get_response(ModelRequest::new(input, settings()).with_tools(tools))
        .await
        .expect("the second turn should succeed after replaying the first turn");
    report("second turn (replay)", &second);
    assert!(
        second
            .output()
            .iter()
            .any(|item| matches!(item.kind(), RunItemKind::Message(_))),
        "the second turn should produce a text reply"
    );
}
