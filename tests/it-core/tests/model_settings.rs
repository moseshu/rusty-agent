//! R1-2b contracts for provider-neutral model settings and four-layer resolution.

use std::time::Duration;

use insta::assert_json_snapshot;
use ra_core::model::{
    Effort, JsonMap, McpToolChoice, ModelRetrySettings, ModelSettings, ProviderKey,
    RetryBackoffSettings, ThinkingConfig, ToolChoice,
};
use serde_json::{Value, json};

fn provider(name: &str) -> ProviderKey {
    ProviderKey::new(name)
}

fn object(value: Value) -> JsonMap {
    value
        .as_object()
        .expect("fixture 应是 JSON object")
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

#[test]
fn test_model_settings_01() {
    let key = provider("openrouter");
    let provider_defaults = ModelSettings::new()
        .with_temperature(0.8)
        .with_max_tokens(8_192)
        .with_timeout(Duration::from_secs(60))
        .with_tool_choice(ToolChoice::Auto)
        .with_parallel_tool_calls(true)
        .with_metadata("source", "provider")
        .with_metadata("provider_only", "yes")
        .with_extra_header("x-layer", "provider")
        .with_extra_query("api-version", "v1")
        .with_extra_body(
            key.clone(),
            object(json!({"route": "provider", "nested": {"provider": true}})),
        )
        .with_retry(
            ModelRetrySettings::new()
                .with_max_retries(3)
                .with_backoff(
                    RetryBackoffSettings::new()
                        .with_initial_delay(Duration::from_millis(100))
                        .with_jitter(true),
                ),
        );
    let agent_defaults = ModelSettings::new()
        .with_temperature(0.0)
        .with_top_p(0.9)
        .with_max_tokens(4_096)
        .with_thinking(ThinkingConfig::Adaptive)
        .with_metadata("source", "agent")
        .with_extra_header("x-agent", "yes")
        .with_retry(
            ModelRetrySettings::new().with_backoff(
                RetryBackoffSettings::new().with_max_delay(Duration::from_secs(5)),
            ),
        );
    let model_defaults = ModelSettings::new()
        .with_frequency_penalty(0.2)
        .with_presence_penalty(0.1)
        .with_max_tokens(2_048)
        .with_timeout(Duration::from_secs(30))
        .with_effort(Effort::XHigh)
        .with_extra_body(
            key.clone(),
            object(json!({"nested": {"model": true}})),
        );
    let run_overrides = ModelSettings::new()
        .with_max_tokens(3_000)
        .with_timeout(Duration::from_secs(45))
        .with_tool_choice(ToolChoice::Required)
        .with_parallel_tool_calls(false)
        .with_metadata("source", "run")
        .with_extra_header("x-layer", "run")
        .with_extra_query("api-version", "v2")
        .with_retry(
            ModelRetrySettings::new()
                .with_max_retries(0)
                .with_backoff(
                    RetryBackoffSettings::new()
                        .with_multiplier(0.0)
                        .with_jitter(false),
                ),
        );

    let resolved = provider_defaults.resolve(
        &key,
        &agent_defaults,
        &model_defaults,
        &run_overrides,
    );

    assert_eq!(resolved.provider(), &key);
    assert_eq!(resolved.max_tokens(), Some(2_048));
    assert_eq!(resolved.timeout(), Some(Duration::from_secs(30)));
    assert_eq!(resolved.extra_headers()["x-layer"], "run");
    assert_eq!(resolved.extra_headers()["x-agent"], "yes");
    assert_eq!(resolved.extra_query()["api-version"], "v2");

    assert_json_snapshot!(resolved.to_traceable_value().expect("trace 投影应可序列化"), @r###"
    {
      "effort": "xhigh",
      "frequency_penalty": 0.2,
      "max_tokens": 2048,
      "metadata": {
        "provider_only": "yes",
        "source": "run"
      },
      "parallel_tool_calls": false,
      "presence_penalty": 0.1,
      "retry": {
        "backoff": {
          "initial_delay": 100,
          "jitter": false,
          "max_delay": 5000,
          "multiplier": 0.0,
          "schema_version": 1
        },
        "max_retries": 0,
        "schema_version": 1
      },
      "schema_version": 1,
      "temperature": 0.0,
      "thinking": {
        "type": "adaptive"
      },
      "timeout": 30000,
      "tool_choice": {
        "type": "required"
      },
      "top_p": 0.9
    }
    "###);
}

#[test]
fn test_model_settings_02() {
    let key = provider("compat");
    let base = ModelSettings::new()
        .with_temperature(0.7)
        .with_parallel_tool_calls(true);
    let unset = ModelSettings::new();
    let explicit = ModelSettings::new()
        .with_temperature(0.0)
        .with_parallel_tool_calls(false);

    let inherited = base.resolve(&key, &unset, &unset, &unset);
    assert_eq!(inherited.temperature(), Some(0.7));
    assert_eq!(inherited.parallel_tool_calls(), Some(true));

    let overridden = base.resolve(&key, &unset, &unset, &explicit);
    assert_eq!(overridden.temperature(), Some(0.0));
    assert_eq!(overridden.parallel_tool_calls(), Some(false));
}

#[test]
fn test_model_settings_03() {
    // Turn preparation resolves settings after tools precisely so this can happen: a selector that
    // survived the four-layer merge may name something this turn no longer advertises. Dynamic
    // availability is a feature, so an unsatisfiable selection degrades instead of ending the run.
    let key = provider("openai");
    let unset = ModelSettings::new();
    let required = ModelSettings::new()
        .with_tool_choice(ToolChoice::Required)
        .with_parallel_tool_calls(true);

    let empty_surface = required
        .resolve(&key, &unset, &unset, &unset)
        .reconcile_tool_surface([]);
    assert_eq!(empty_surface.tool_choice(), None);
    assert_eq!(empty_surface.parallel_tool_calls(), None);

    let live_surface = required
        .resolve(&key, &unset, &unset, &unset)
        .reconcile_tool_surface(["read_file"]);
    assert_eq!(live_surface.tool_choice(), Some(&ToolChoice::Required));
    assert_eq!(live_surface.parallel_tool_calls(), Some(true));

    let pinned = ModelSettings::new().with_tool_choice(ToolChoice::Tool("read_file".to_owned()));
    assert_eq!(
        pinned
            .resolve(&key, &unset, &unset, &unset)
            .reconcile_tool_surface(["read_file", "write_file"])
            .tool_choice(),
        Some(&ToolChoice::Tool("read_file".to_owned()))
    );
    assert_eq!(
        pinned
            .resolve(&key, &unset, &unset, &unset)
            .reconcile_tool_surface(["write_file"])
            .tool_choice(),
        None
    );

    // Two selections outlive an empty surface: "no tools" stays true, and a server-hosted tool
    // never shows up in the neutral surface to begin with.
    let none = ModelSettings::new().with_tool_choice(ToolChoice::None);
    assert_eq!(
        none.resolve(&key, &unset, &unset, &unset)
            .reconcile_tool_surface([])
            .tool_choice(),
        Some(&ToolChoice::None)
    );
    let hosted = ModelSettings::new()
        .with_tool_choice(ToolChoice::Mcp(McpToolChoice::new("docs", "search")));
    assert_eq!(
        hosted
            .resolve(&key, &unset, &unset, &unset)
            .reconcile_tool_surface([])
            .tool_choice(),
        Some(&ToolChoice::Mcp(McpToolChoice::new("docs", "search")))
    );
}

#[test]
fn test_model_settings_04() {
    // A timeout is a latency bound, not a capability: any layer wanting to wait less has standing,
    // and the request is simply abandoned sooner.
    let key = provider("anthropic");
    let provider_defaults = ModelSettings::new().with_timeout(Duration::from_secs(60));
    let agent_defaults = ModelSettings::new().with_timeout(Duration::from_secs(15));
    let model_defaults = ModelSettings::new();
    let run_overrides = ModelSettings::new().with_timeout(Duration::from_secs(30));

    let resolved =
        provider_defaults.resolve(&key, &agent_defaults, &model_defaults, &run_overrides);
    assert_eq!(resolved.timeout(), Some(Duration::from_secs(15)));
}

#[test]
fn test_model_settings_05() {
    // Only the model layer states a hard fact. An agent writing max_tokens means "usually enough",
    // not "never more"; taking the min across all four layers would silently erase the value a run
    // set explicitly because it wanted a long answer.
    let key = provider("anthropic");
    let empty = ModelSettings::new();
    let model_limit = ModelSettings::new().with_max_tokens(8_192);
    let agent_preference = ModelSettings::new().with_max_tokens(1_000);

    let raised = empty.resolve(
        &key,
        &agent_preference,
        &model_limit,
        &ModelSettings::new().with_max_tokens(4_000),
    );
    assert_eq!(raised.max_tokens(), Some(4_000), "run 应能高于 agent 默认");

    let capped = empty.resolve(
        &key,
        &empty,
        &model_limit,
        &ModelSettings::new().with_max_tokens(16_384),
    );
    assert_eq!(capped.max_tokens(), Some(8_192), "超过模型上限必须被截住");

    let inherited = empty.resolve(&key, &agent_preference, &model_limit, &empty);
    assert_eq!(inherited.max_tokens(), Some(1_000), "run 未设时沿用 agent 默认");

    let uncapped = empty.resolve(&key, &agent_preference, &empty, &empty);
    assert_eq!(uncapped.max_tokens(), Some(1_000), "没有模型上限就不封顶");
}

#[test]
fn test_model_settings_06() {
    // The provider fallback exists because "some endpoints require this field" (Anthropic does),
    // which is no reason to override a per-model value written precisely because that model
    // differs — the coarsest layer beating the most specific one is backwards.
    let key = provider("anthropic");
    let empty = ModelSettings::new();
    let provider_fallback = ModelSettings::new().with_max_tokens(4_096);
    let model_limit = ModelSettings::new().with_max_tokens(64_000);

    assert_eq!(
        provider_fallback
            .resolve(&key, &empty, &model_limit, &empty)
            .max_tokens(),
        Some(64_000),
        "更具体的模型注册应胜出"
    );
    assert_eq!(
        provider_fallback
            .resolve(&key, &empty, &empty, &empty)
            .max_tokens(),
        Some(4_096),
        "模型没自己说时兜底才生效"
    );
    assert_eq!(
        provider_fallback
            .resolve(
                &key,
                &ModelSettings::new().with_max_tokens(1_000),
                &model_limit,
                &empty
            )
            .max_tokens(),
        Some(1_000),
        "用户意图压过两种注册方默认"
    );
}

#[test]
fn test_model_settings_07() {
    let key = provider("vllm");
    let provider_defaults = ModelSettings::new().with_extra_body(
        key.clone(),
        object(json!({
            "sampling": {"top_k": 40, "nested": {"a": 1}},
            "provider_only": true
        })),
    );
    let agent_defaults = ModelSettings::new().with_extra_body(
        key.clone(),
        object(json!({
            "sampling": {"top_k": 20, "min_p": 0.1, "nested": {"b": 2}},
            "agent_only": true
        })),
    );
    let model_defaults = ModelSettings::new().with_extra_body(
        key.clone(),
        object(json!({
            "sampling": {"nested": {"a": 3, "c": 4}},
            "model_only": true
        })),
    );
    let run_overrides = ModelSettings::new().with_extra_body(
        key.clone(),
        object(json!({
            "sampling": {"min_p": 0.2},
            "run_only": true
        })),
    );
    let originals = [
        provider_defaults.clone(),
        agent_defaults.clone(),
        model_defaults.clone(),
        run_overrides.clone(),
    ];

    let resolved = provider_defaults.resolve(
        &key,
        &agent_defaults,
        &model_defaults,
        &run_overrides,
    );

    assert_eq!(
        resolved.extra_body(),
        &object(json!({
            "sampling": {
                "top_k": 20,
                "min_p": 0.2,
                "nested": {"a": 3, "b": 2, "c": 4}
            },
            "provider_only": true,
            "agent_only": true,
            "model_only": true,
            "run_only": true
        }))
    );
    assert_eq!(
        originals,
        [
            provider_defaults,
            agent_defaults,
            model_defaults,
            run_overrides
        ],
        "resolve 不得修改任何输入层"
    );
}

#[test]
fn test_model_settings_08() {
    let vllm = provider("vllm");
    let openrouter = provider("openrouter");
    let settings = ModelSettings::new()
        .with_extra_body(
            vllm.clone(),
            object(json!({"guided_json": {"type": "object"}, "top_k": 20})),
        )
        .with_extra_body(
            openrouter.clone(),
            object(json!({"provider": {"order": ["A", "B"]}, "route": "fallback"})),
        );
    let empty = ModelSettings::new();

    let for_vllm = settings.resolve(&vllm, &empty, &empty, &empty);
    let for_openrouter = settings.resolve(&openrouter, &empty, &empty, &empty);

    assert!(for_vllm.extra_body().contains_key("guided_json"));
    assert!(!for_vllm.extra_body().contains_key("provider"));
    assert!(for_openrouter.extra_body().contains_key("provider"));
    assert!(!for_openrouter.extra_body().contains_key("guided_json"));
}

#[test]
fn test_model_settings_09() {
    let key = provider("compat");
    let settings = ModelSettings::new()
        .with_temperature(0.5)
        .with_metadata("safe", "visible")
        .with_extra_header("authorization", "header-secret")
        .with_extra_query("api-key", "query-secret")
        .with_extra_body(
            key.clone(),
            object(json!({"private_token": "body-secret"})),
        );
    let empty = ModelSettings::new();
    let resolved = settings.resolve(&key, &empty, &empty, &empty);

    let trace = resolved
        .to_traceable_value()
        .expect("trace 投影应可序列化");
    let encoded = serde_json::to_string(&trace).expect("trace JSON 应可序列化");
    assert_eq!(trace["temperature"], 0.5);
    assert_eq!(trace["metadata"]["safe"], "visible");
    for forbidden in [
        "header-secret",
        "query-secret",
        "body-secret",
        "extra_headers",
        "extra_query",
        "extra_body",
    ] {
        assert!(!encoded.contains(forbidden), "trace 泄漏了 {forbidden}");
    }
}

#[test]
fn test_model_settings_10() {
    let key = provider("openai");
    let provider_defaults = ModelSettings::new().with_retry(
        ModelRetrySettings::new()
            .with_max_retries(3)
            .with_backoff(
                RetryBackoffSettings::new()
                    .with_initial_delay(Duration::from_millis(250))
                    .with_max_delay(Duration::from_secs(4))
                    .with_jitter(true),
            ),
    );
    let run_overrides = ModelSettings::new().with_retry(
        ModelRetrySettings::new()
            .with_max_retries(0)
            .with_backoff(
                RetryBackoffSettings::new()
                    .with_multiplier(0.0)
                    .with_jitter(false),
            ),
    );
    let empty = ModelSettings::new();
    let resolved = provider_defaults.resolve(&key, &empty, &empty, &run_overrides);
    let retry = resolved.retry().expect("retry 应存在");
    let backoff = retry.backoff().expect("backoff 应存在");

    assert_eq!(retry.max_retries(), Some(0));
    assert_eq!(backoff.initial_delay(), Some(Duration::from_millis(250)));
    assert_eq!(backoff.max_delay(), Some(Duration::from_secs(4)));
    assert_eq!(backoff.multiplier(), Some(0.0));
    assert_eq!(backoff.jitter(), Some(false));
}

#[test]
fn test_model_settings_11() {
    // A merge starts from an empty object, so anything not carried over explicitly ends up knowing
    // less than the layer it came from — exactly what Unknown exists to prevent.
    let key = provider("compat");
    let newer_layer: ModelRetrySettings = serde_json::from_value(json!({
        "schema_version": 1,
        "max_retries": 2,
        "future_retry_knob": {"enabled": true}
    }))
    .expect("新版 retry 应能被旧代码读取");
    let settings = ModelSettings::new().with_retry(newer_layer);
    let empty = ModelSettings::new();

    let resolved = settings.resolve(&key, &empty, &empty, &empty);
    let retry = resolved.retry().expect("retry 应存在");

    assert_eq!(retry.max_retries(), Some(2));
    assert_eq!(
        retry.unknown().get("future_retry_knob"),
        Some(&json!({"enabled": true}))
    );
}

#[test]
fn test_model_settings_12() {
    // schema_version already promises "obvious to a reader in any language", and Duration's
    // default {"secs":N,"nanos":M} does not meet that bar.
    let settings = ModelSettings::new()
        .with_timeout(Duration::from_millis(1_500))
        .with_retry(ModelRetrySettings::new().with_backoff(
            RetryBackoffSettings::new().with_initial_delay(Duration::from_millis(250)),
        ));

    let encoded = serde_json::to_value(&settings).expect("配置应可序列化");
    assert_eq!(encoded["timeout"], 1_500);
    assert_eq!(encoded["retry"]["backoff"]["initial_delay"], 250);

    let decoded: ModelSettings = serde_json::from_value(encoded).expect("应可读回");
    assert_eq!(decoded.timeout(), Some(Duration::from_millis(1_500)));
    assert_eq!(decoded, settings);
}

#[test]
fn test_model_settings_13() {
    let key = provider("compat");
    let settings = ModelSettings::new()
        .with_temperature(0.0)
        .with_extra_body_value(key, "top_k", 0);
    let mut value = serde_json::to_value(settings).expect("配置应可序列化");
    value
        .as_object_mut()
        .expect("settings 应是对象")
        .insert("future_setting".into(), json!({"enabled": false}));

    let decoded: ModelSettings = serde_json::from_value(value).expect("旧代码应能读取");
    assert_eq!(decoded.unknown().get("future_setting"), Some(&json!({"enabled": false})));
    let rewritten = serde_json::to_value(decoded).expect("旧代码应能回写");
    assert_eq!(rewritten["future_setting"]["enabled"], false);
    let first = serde_json::to_string(&rewritten).expect("应可序列化");
    let second = serde_json::to_string(&rewritten).expect("应可再次序列化");
    assert_eq!(first, second);
}
