// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
use ra_coding::plugins::PluginId;

#[test]
fn accepts_dotted_plugin_names() {
    let plugin_id =
        PluginId::new("acme.tools".to_string(), "marketplace".to_string()).expect("plugin id");

    assert_eq!(plugin_id.as_key(), "acme.tools@marketplace");
}

#[test]
fn marketplace_names_preserve_legacy_character_set() {
    PluginId::new("acme".to_string(), "marketplace_name-1".to_string()).expect("marketplace name");

    let err = PluginId::new("acme".to_string(), "market.place".to_string())
        .expect_err("dotted marketplace name");
    assert_eq!(
        err.to_string(),
        "invalid marketplace name: only ASCII letters, digits, `_`, and `-` are allowed"
    );
}

#[test]
fn rejects_dot_path_segments() {
    assert!(PluginId::new(".".to_string(), "marketplace".to_string()).is_err());
    assert!(PluginId::new("plugin".to_string(), "..".to_string()).is_err());
}

#[test]
fn rejects_dots_that_can_alias_path_segments() {
    for plugin_name in [".plugin", "plugin.", "plugin..name"] {
        assert!(PluginId::new(plugin_name.to_string(), "marketplace".to_string()).is_err());
    }
}

#[test]
fn key_parse_keeps_upstream_error_context_and_last_separator() {
    for key in [
        "missing",
        "@market",
        "plugin@",
        "a@b@market",
        "a/b@market",
        "plugin@market.place",
    ] {
        let error = PluginId::parse(key).unwrap_err();
        assert!(error.to_string().contains(key), "{key}: {error}");
    }
    let parsed = PluginId::parse("acme.tools@market_name-1").unwrap();
    assert_eq!(parsed.plugin_name, "acme.tools");
    assert_eq!(parsed.marketplace_name, "market_name-1");
    assert_eq!(parsed.as_key(), "acme.tools@market_name-1");
}
