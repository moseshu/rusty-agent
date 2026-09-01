//! Model-window lookup drives proportional compaction without guessing unknown capacities.

use std::collections::BTreeMap;

use ra_context::window::{
    ContextWindowConfig, ContextWindowTable, ContextWindowThresholdRatio,
    DEFAULT_COMPACTION_THRESHOLD_RATIO,
};

#[test]
fn built_in_models_follow_the_reference_table_after_normalization() {
    let table = ContextWindowTable::default();

    assert_eq!(table.context_window(" openai/GPT-5.4-PRO "), Some(1_047_576));
    assert_eq!(table.context_window("gpt-5.3-codex"), Some(400_000));
    assert_eq!(table.context_window("o4-mini-deep-research"), Some(200_000));
    assert_eq!(table.context_window("gpt-4o-mini"), Some(128_000));
    assert_eq!(table.context_window("other/gpt-5.4"), None);
}

#[test]
fn default_trigger_is_sixty_percent_of_the_resolved_window() {
    let config = ContextWindowConfig::default();

    assert_eq!(DEFAULT_COMPACTION_THRESHOLD_RATIO.basis_points(), 6_000);
    assert_eq!(config.compaction_threshold("gpt-5.3-codex"), Some(240_000));
    assert_eq!(config.compaction_threshold("gpt-4o"), Some(76_800));
    assert_eq!(config.compaction_threshold("unknown-model"), None);
}

#[test]
fn host_overrides_replace_builtin_windows_and_add_unknown_models() {
    let config = ContextWindowConfig::new(
        BTreeMap::from([
            ("openai/gpt-5.3-codex".to_owned(), 512_000),
            ("local/agent-model".to_owned(), 32_001),
        ]),
        ContextWindowThresholdRatio::from_basis_points(6_250),
    )
    .expect("valid overrides");

    assert_eq!(config.context_window("gpt5.3_codex"), None);
    assert_eq!(config.context_window("gpt-5.3-codex"), Some(512_000));
    assert_eq!(config.compaction_threshold("gpt-5.3-codex"), Some(320_000));
    assert_eq!(config.context_window("local/agent-model"), Some(32_001));
    assert_eq!(config.compaction_threshold("local/agent-model"), Some(20_000));
}

#[test]
fn serialized_config_validates_overrides_and_ratio() {
    let config: ContextWindowConfig = serde_json::from_str(
        r#"{
            "context_windows": {"vendor/model-1": 8192},
            "compaction_threshold_ratio": 0.625
        }"#,
    )
    .expect("a valid configuration");

    assert_eq!(config.context_window("vendor/model.1"), Some(8_192));
    assert_eq!(config.compaction_threshold("vendor/model-1"), Some(5_120));
    assert_eq!(
        serde_json::to_value(config).expect("configuration serializes")["compaction_threshold_ratio"],
        serde_json::json!(0.625)
    );

    for invalid in [
        r#"{"context_windows":{"new-model":0}}"#,
        r#"{"context_windows":{"  ":4096}}"#,
        r#"{"context_windows":{"gpt-5":1,"gpt5":2}}"#,
        r#"{"compaction_threshold_ratio":1.00001}"#,
        r#"{"unexpected":true}"#,
    ] {
        assert!(serde_json::from_str::<ContextWindowConfig>(invalid).is_err());
    }
}

#[test]
fn two_decimal_ratios_survive_binary_rounding() {
    // `0.56 * 10_000` lands on 5600.000000000001, so the whole-number check has to tolerate the
    // rounding error of the decimal's binary form rather than demand an exact integer.
    for (text, basis_points) in [
        ("0.56", 5_600_u16),
        ("0.57", 5_700),
        ("0.68", 6_800),
        ("0.69", 6_900),
        ("0.81", 8_100),
    ] {
        let ratio: ContextWindowThresholdRatio =
            serde_json::from_str(text).expect("a two-decimal ratio parses");
        assert_eq!(ratio.basis_points(), basis_points);
    }

    // A zero threshold asks for compaction before every request, and no history can satisfy that.
    // It is refused where it is written rather than at every later trigger resolution.
    assert!(ContextWindowThresholdRatio::new(0).is_err());
    for invalid in ["0.60001", "0.123456", "1.00001", "-0.1", "0.0", "0"] {
        assert!(
            serde_json::from_str::<ContextWindowThresholdRatio>(invalid).is_err(),
            "`{invalid}` should not parse as a threshold ratio"
        );
    }
}

#[test]
fn config_round_trips_through_its_own_serialized_form() {
    let config = ContextWindowConfig::new(
        BTreeMap::from([("vendor/model-1".to_owned(), 8_192)]),
        ContextWindowThresholdRatio::from_basis_points(5_600),
    )
    .expect("valid overrides");

    let document = serde_json::to_string(&config).expect("configuration serializes");
    let restored: ContextWindowConfig =
        serde_json::from_str(&document).expect("its own output parses back");

    assert_eq!(restored, config);
    assert_eq!(restored.context_window("vendor/model.1"), Some(8_192));
    assert_eq!(restored.compaction_threshold("vendor/model-1"), Some(4_587));
}

#[test]
fn config_and_its_table_resolve_every_name_shape_alike() {
    let config = ContextWindowConfig::new(
        BTreeMap::from([("local/agent-model".to_owned(), 32_001)]),
        DEFAULT_COMPACTION_THRESHOLD_RATIO,
    )
    .expect("valid overrides");

    for model in [
        "gpt-5.3-codex",
        " openai/GPT-5.4-PRO ",
        "local/agent-model",
        "open-ai/gpt-5",
        "unknown-model",
    ] {
        assert_eq!(
            config.context_window(model),
            config.table().context_window(model),
            "`{model}` resolved differently through the config and through its table"
        );
    }

    // `openai/` is stripped once, before the separators are dropped, so a name that only looks
    // like the prefix after normalization stays unknown.
    assert_eq!(config.context_window("open-ai/gpt-5"), None);
}
