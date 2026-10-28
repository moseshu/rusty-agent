// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
use ra_coding::plugins::toggles::collect_plugin_enabled_candidates;
use serde_json::json;
use std::collections::BTreeMap;

#[test]
fn collect_plugin_enabled_candidates_tracks_direct_and_table_writes() {
    let candidates = collect_plugin_enabled_candidates(
        [
            (&"plugins.sample@test.enabled".to_string(), &json!(true)),
            (
                &"plugins.other@test".to_string(),
                &json!({ "enabled": false, "ignored": true }),
            ),
            (
                &"plugins".to_string(),
                &json!({
                    "nested@test": { "enabled": true },
                    "skip@test": { "name": "skip" },
                }),
            ),
        ]
        .into_iter(),
    );

    assert_eq!(
        candidates,
        BTreeMap::from([
            ("nested@test".to_string(), true),
            ("other@test".to_string(), false),
            ("sample@test".to_string(), true),
        ])
    );
}

#[test]
fn collect_plugin_enabled_candidates_uses_last_write_for_same_plugin() {
    let candidates = collect_plugin_enabled_candidates(
        [
            (&"plugins.sample@test.enabled".to_string(), &json!(true)),
            (
                &"plugins.sample@test".to_string(),
                &json!({ "enabled": false }),
            ),
        ]
        .into_iter(),
    );

    assert_eq!(
        candidates,
        BTreeMap::from([("sample@test".to_string(), false)])
    );
}
