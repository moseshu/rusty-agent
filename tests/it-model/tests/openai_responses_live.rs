//! 打真实端点的冒烟测试。默认 `#[ignore]`，离线 CI 不会跑到。
//!
//! 运行：
//! ```text
//! RA_LIVE_OPENAI_BASE_URL=https://api.bianxie.ai/v1 \
//! RA_LIVE_OPENAI_API_KEY="$OPENAI_API_KEY1" \
//! RA_LIVE_OPENAI_MODEL=gpt-5.5 \
//! cargo test --manifest-path tests/Cargo.toml -p it-model --test openai_responses_live -- --ignored --nocapture
//! ```
//!
//! 这些用例的价值不在断言，而在**打印出来的实测事实**：第三方端点认不认
//! `include: reasoning.encrypted_content`、reasoning 能不能原样回放、`cached_tokens` 与
//! `x-request-id` 透不透传。结论应当回写进 R1-6b 的 `Quirks` 与 R1-8 的 usage 明细。

use ra_core::{
    item::{ModelInputItem, RunItemKind, ToolCallOutput},
    model::{Effort, Model, ModelRequest, ModelSettings, ModelToolDefinition, ProviderKey},
};
use ra_model::openai::{auth::OpenAiAuth, responses::OpenAiResponsesModel};
use serde_json::json;

fn required_env(name: &str) -> String {
    std::env::var(name)
        .unwrap_or_else(|_| panic!("实测用例需要环境变量 {name}；见本文件顶部的运行命令"))
}

fn live_model() -> OpenAiResponsesModel {
    OpenAiResponsesModel::new(
        required_env("RA_LIVE_OPENAI_MODEL"),
        OpenAiAuth::new(required_env("RA_LIVE_OPENAI_API_KEY"))
            .with_base_url(required_env("RA_LIVE_OPENAI_BASE_URL")),
    )
    .expect("实测模型应当能构造")
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

/// 一轮最小往返：请求形状能否被真实服务端接受。
#[tokio::test]
#[ignore = "打真实端点，需要凭据"]
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
        .expect("最小往返应当成功");

    report("最小往返", &response);
    assert!(response.response_id().is_some(), "服务端应返回 response id");
    assert!(response.usage().input_tokens() > 0, "usage 应当有输入 token");
}

/// 两轮：把第一轮的 reasoning 与 tool call 原样回放，验证 `encrypted_content` 回传路径。
///
/// 这是 R1-4 最容易在真实端点上翻车的一条：`store=false` 时 reasoning 必须靠
/// `include: reasoning.encrypted_content` 拿到回放材料，中转站很可能把它吞掉。
#[tokio::test]
#[ignore = "打真实端点，需要凭据"]
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

    // effort 拉高 + 需要推演的问法：只有这样第一轮才会同时产出 reasoning 与工具调用，
    // 回放路径才真正被走到。简单问句会让模型直接调工具、reasoning 为 0，测不出东西。
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
        .expect("第一轮应当成功");
    report("第一轮（期望 reasoning + 工具调用）", &first);

    let call_id = first
        .output()
        .iter()
        .find_map(|item| match item.kind() {
            RunItemKind::ToolCall(call) => Some(call.call_id().clone()),
            _ => None,
        })
        .expect("第一轮应当产生一次工具调用");
    // 模型这一轮出不出 reasoning 由它自己决定，不作断言；但只要出了，就必须带回放材料——
    // 缺了它就说明中转站把 `include: reasoning.encrypted_content` 吞了，属于 R1-6b 的 quirk。
    let reasoning_items = first
        .output()
        .iter()
        .filter_map(|item| match item.kind() {
            RunItemKind::Reasoning(reasoning) => Some(reasoning),
            _ => None,
        })
        .collect::<Vec<_>>();
    eprintln!("reasoning 项数 = {}", reasoning_items.len());
    assert!(
        reasoning_items
            .iter()
            .all(|reasoning| reasoning.encrypted_content().is_some()),
        "端点回传了 reasoning 却没有 encrypted_content：store=false 下无法回放"
    );

    let mut input = first.to_input_items();
    input.push(ModelInputItem::ToolCallOutput(ToolCallOutput::new(
        call_id,
        json!({"temp_c": 21, "condition": "clear"}),
    )));

    let second = model
        .get_response(ModelRequest::new(input, settings()).with_tools(tools))
        .await
        .expect("回放第一轮的 reasoning 与工具调用后，第二轮应当成功");
    report("第二轮（回放）", &second);
    assert!(
        second
            .output()
            .iter()
            .any(|item| matches!(item.kind(), RunItemKind::Message(_))),
        "第二轮应当给出文本答复"
    );
}
