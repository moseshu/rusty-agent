//! `ra-tools::sandbox::compaction`: the policies, the model table, the request field and the input
//! it keeps.
//!
//! Ported from the reference's `tests/sandbox/test_compaction.py` and
//! `tests/sandbox/capabilities/test_compaction_capability.py`. The reference reads the request field
//! back from the dictionary `sampling_params` returns; here it lands in the resolved provider's
//! `extra_body` bucket, so that is where these tests read it.

use async_trait::async_trait;
use ra_core::{
    capability::{
        Capability, CapabilityFamily, ContextProcessorRequest, ContextSummarizer,
        ContextSummaryRequest, ContextSummaryResponse, SamplingContext,
    },
    error::{Error, Result},
    item::{ItemId, Message, ModelInputItem, ProviderCompaction},
    model::{ModelSettings, ProviderKey},
    state::RunId,
};
use ra_tools::sandbox::compaction::{
    Compaction, CompactionModelInfo, CompactionPolicy, DEFAULT_COMPACT_THRESHOLD,
};
use rstest::rstest;
use serde_json::{Value, json};

fn provider() -> ProviderKey {
    ProviderKey::new("openai")
}

/// The field a capability wrote for `model`, read back from the provider's bucket.
fn context_management(capability: &Compaction, model: Option<&str>) -> Value {
    let mut context = SamplingContext::new().with_provider(provider());
    if let Some(model) = model {
        context = context.with_model(model);
    }
    let settings = capability.sampling_params_for(ModelSettings::new(), &context);
    settings.extra_body()[&provider()]["context_management"].clone()
}

fn threshold(threshold: u64) -> Value {
    json!([{"type": "compaction", "compact_threshold": threshold}])
}

fn compaction_item(summary: &str) -> ModelInputItem {
    ModelInputItem::ProviderCompaction(ProviderCompaction::new(
        "openai",
        json!({"type": "compaction", "summary": summary}),
    ))
}

// --- test_compaction.py ---------------------------------------------------------------------------

/// `test_compaction_model_info_for_model_returns_context_window`, every parameter.
#[rstest]
#[case("gpt-5.4", 1_047_576)]
#[case("gpt-5.4-pro", 1_047_576)]
#[case("gpt-5.5", 1_047_576)]
#[case("gpt-5.5-2026-04-23", 1_047_576)]
#[case("gpt-5.5-pro", 1_047_576)]
#[case("gpt-5.5-pro-2026-04-23", 1_047_576)]
#[case("gpt-5.6", 1_047_576)]
#[case("gpt-5.6-sol", 1_047_576)]
#[case("gpt-5.6-terra", 1_047_576)]
#[case("gpt-5.6-luna", 1_047_576)]
#[case("gpt-5.3-codex", 400_000)]
#[case("gpt-5.4-mini", 400_000)]
#[case("gpt-4.1", 1_047_576)]
#[case("o3", 200_000)]
#[case("gpt-4o", 128_000)]
#[case("openai/gpt-5.4", 1_047_576)]
#[case("openai/gpt-5.5", 1_047_576)]
#[case("gpt-5-2", 400_000)]
#[case("gpt-5-4", 1_047_576)]
#[case("gpt-5-5", 1_047_576)]
#[case("openai/gpt-5-4-mini", 400_000)]
#[case("gpt-4-1-mini", 1_047_576)]
fn a_known_model_has_its_context_window(#[case] model: &str, #[case] window: u64) {
    assert_eq!(
        CompactionModelInfo::for_model(model)
            .expect("listed")
            .context_window(),
        window
    );
}

/// `test_compaction_model_info_for_model_rejects_unknown_model`.
#[test]
fn an_unknown_model_is_refused_by_name() {
    let error = CompactionModelInfo::for_model("not-a-model").expect_err("unlisted");
    assert!(
        error
            .to_string()
            .contains("Unknown context window for model: 'not-a-model'"),
        "{error}"
    );
}

/// `test_compaction_model_info_maybe_for_model_returns_none_for_unknown_model`.
#[test]
fn an_unknown_model_has_no_entry() {
    assert_eq!(CompactionModelInfo::maybe_for_model("not-a-model"), None);
}

#[test]
fn the_lookup_ignores_case_padding_and_the_openai_prefix() {
    assert_eq!(
        CompactionModelInfo::maybe_for_model("  OpenAI/GPT-4O  ").map(|info| info.context_window()),
        Some(128_000)
    );
}

// --- test_compaction_capability.py ----------------------------------------------------------------

/// `test_sampling_params_uses_static_threshold`.
#[test]
fn a_static_policy_asks_for_its_threshold() {
    let capability = Compaction::with_policy(CompactionPolicy::static_threshold(123));
    assert_eq!(context_management(&capability, None), threshold(123));
    assert_eq!(
        capability.policy(),
        Some(&CompactionPolicy::static_threshold(123))
    );
}

/// `test_sampling_params_infers_hyphenated_model_threshold`.
#[test]
fn a_known_model_compacts_at_ninety_percent_of_its_window() {
    assert_eq!(
        context_management(&Compaction::new(), Some("gpt-5-2")),
        threshold(360_000)
    );
}

/// `test_sampling_params_infers_gpt_5_6_sol_dynamic_threshold`.
#[test]
fn the_share_is_truncated_to_a_whole_token() {
    assert_eq!(
        context_management(&Compaction::new(), Some("gpt-5.6-sol")),
        threshold(942_818)
    );
}

/// `test_sampling_params_falls_back_for_unknown_model`.
#[test]
fn an_unknown_model_falls_back_to_the_static_default() {
    assert_eq!(
        context_management(&Compaction::new(), Some("azure-prod-deployment")),
        threshold(DEFAULT_COMPACT_THRESHOLD)
    );
    assert_eq!(DEFAULT_COMPACT_THRESHOLD, 240_000);
}

#[test]
fn an_unnamed_model_falls_back_to_the_static_default() {
    assert_eq!(
        context_management(&Compaction::new(), None),
        threshold(240_000)
    );
    assert_eq!(
        context_management(&Compaction::new(), Some("")),
        threshold(240_000)
    );
}

/// `test_process_context_keeps_items_from_last_compaction`.
#[test]
fn the_input_is_kept_from_the_last_compaction_on() {
    let input = vec![
        ModelInputItem::Message(Message::user("old-1")),
        compaction_item("first"),
        ModelInputItem::Message(Message::user("between")),
        compaction_item("second"),
        ModelInputItem::Message(Message::user("latest")),
    ];
    assert_eq!(Compaction::process_input(&input), input[3..].to_vec());
}

/// `test_process_context_returns_original_when_no_compaction`.
#[test]
fn an_input_without_a_compaction_is_left_alone() {
    let input = vec![
        ModelInputItem::Message(Message::user("hello")),
        ModelInputItem::Message(Message::user("world")),
    ];
    assert_eq!(Compaction::process_input(&input), input);
}

/// `test_rejects_unsupported_policy_type`.
#[test]
fn an_unsupported_policy_type_is_refused_by_name() {
    let error = CompactionPolicy::from_value(&json!({"type": "unknown"})).expect_err("unknown");
    assert!(
        error
            .to_string()
            .contains("Unsupported compaction policy type: 'unknown'"),
        "{error}"
    );
    let error = CompactionPolicy::from_value(&json!({})).expect_err("untyped");
    assert!(
        error
            .to_string()
            .contains("Unsupported compaction policy type: None"),
        "{error}"
    );
}

// --- beyond the reference's tests -----------------------------------------------------------------

#[test]
fn a_policy_reads_back_with_the_reference_defaults() {
    assert_eq!(
        CompactionPolicy::from_value(&json!({"type": "static"})).expect("static"),
        CompactionPolicy::static_threshold(240_000)
    );
    assert_eq!(
        CompactionPolicy::from_value(
            &json!({"type": "dynamic", "model_info": {"context_window": 1000}})
        )
        .expect("dynamic"),
        CompactionPolicy::dynamic_default(CompactionModelInfo::new(1000))
    );
    assert_eq!(
        CompactionPolicy::from_value(&json!({
            "type": "dynamic", "model_info": {"context_window": 1000}, "threshold": 0.5
        }))
        .expect("dynamic")
        .compaction_threshold(),
        500
    );
}

#[test]
fn a_dynamic_share_outside_zero_to_one_is_refused() {
    let info = CompactionModelInfo::new(1000);
    assert!(CompactionPolicy::dynamic(info, 1.5).is_err());
    assert!(CompactionPolicy::dynamic(info, -0.1).is_err());
    assert!(CompactionPolicy::dynamic(info, 1.0).is_ok());
    assert!(
        CompactionPolicy::from_value(&json!({
            "type": "dynamic", "model_info": {"context_window": 1000}, "threshold": 2
        }))
        .is_err()
    );
}

#[test]
fn a_policy_serializes_with_its_type() {
    assert_eq!(
        serde_json::to_value(CompactionPolicy::static_threshold(7)).expect("json"),
        json!({"type": "static", "threshold": 7})
    );
    assert_eq!(
        serde_json::to_value(CompactionPolicy::dynamic_default(CompactionModelInfo::new(
            10
        )))
        .expect("json"),
        json!({"type": "dynamic", "model_info": {"context_window": 10}, "threshold": 0.9})
    );
}

#[test]
fn the_field_goes_in_the_resolved_providers_bucket_and_nowhere_else() {
    let settings =
        ModelSettings::new().with_extra_body_value(ProviderKey::new("openai"), "store", false);
    let settings = Compaction::new()
        .sampling_params_for(settings, &SamplingContext::new().with_provider(provider()));
    assert_eq!(settings.extra_body().len(), 1);
    let bucket = &settings.extra_body()[&provider()];
    assert_eq!(bucket["store"], json!(false), "what was there is kept");
    assert_eq!(bucket["context_management"], threshold(240_000));
}

#[test]
fn without_a_provider_there_is_nowhere_to_put_the_field() {
    let settings = Compaction::new().sampling_params_for(
        ModelSettings::new(),
        &SamplingContext::new().with_model("gpt-4o"),
    );
    assert!(settings.extra_body().is_empty());
    assert!(
        Compaction::new()
            .sampling_params(ModelSettings::new())
            .extra_body()
            .is_empty()
    );
}

#[test]
fn compaction_is_its_own_family_and_contributes_no_tools() {
    let capability = Compaction::new();
    assert_eq!(capability.kind(), CapabilityFamily::COMPACTION);
    assert!(capability.tools().is_empty());
    assert!(capability.context_processor().is_some());
}

struct NoSummaries;

#[async_trait]
impl ContextSummarizer for NoSummaries {
    async fn summarize(&self, _request: ContextSummaryRequest) -> Result<ContextSummaryResponse> {
        Err(Error::caller(
            "compaction asks the provider, never the runner",
        ))
    }
}

#[tokio::test]
async fn the_processor_keeps_the_request_input_from_the_last_compaction_on() {
    let input = vec![
        ModelInputItem::Message(Message::user("old")),
        compaction_item("only"),
        ModelInputItem::Message(Message::user("new")),
    ];
    let request = ContextProcessorRequest::new(
        RunId::new("run"),
        1,
        ItemId::new("context-1.0"),
        Some("gpt-4o".to_owned()),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        input.clone(),
    );
    let capability = Compaction::new();
    let result = capability
        .context_processor()
        .expect("a processor")
        .process_context(request, &NoSummaries)
        .await
        .expect("processed");
    assert_eq!(result.input(), &input[1..]);
    assert!(result.generated_items().is_empty());
    assert!(result.model_responses().is_empty());
}

#[test]
fn serde_policy_parsing_preserves_defaults_and_validation() {
    for value in [
        json!({"type": "static"}),
        json!({"type": "dynamic", "model_info": {"context_window": 1000}}),
        json!({"type": "dynamic", "model_info": {"context_window": 1000}, "threshold": 0.0}),
        json!({"type": "dynamic", "model_info": {"context_window": 1000}, "threshold": 1.0}),
    ] {
        let expected = CompactionPolicy::from_value(&value).unwrap();
        let actual: CompactionPolicy = serde_json::from_value(value).unwrap();
        assert_eq!(actual, expected);
        assert_eq!(
            serde_json::from_value::<CompactionPolicy>(serde_json::to_value(&actual).unwrap())
                .unwrap(),
            actual
        );
    }
    for threshold in [-0.1, 2.0] {
        let value = json!({"type": "dynamic", "model_info": {"context_window": 1000}, "threshold": threshold});
        assert!(serde_json::from_value::<CompactionPolicy>(value).is_err());
    }
    assert!(serde_json::from_value::<CompactionPolicy>(json!({"type": "unknown"})).is_err());
}
