// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
use std::collections::HashSet;

use ra_protocol::server::protocol::UserInput;

use ra_coding::plugins::PluginCapabilitySummary;
use ra_coding::plugins::collect_explicit_app_ids;
use ra_coding::plugins::collect_explicit_plugin_ids;
use ra_coding::plugins::collect_explicit_plugin_mentions;

fn text_input(text: &str) -> UserInput {
    UserInput::text(text)
}

fn plugin(config_name: &str, display_name: &str) -> PluginCapabilitySummary {
    PluginCapabilitySummary {
        config_name: config_name.to_string(),
        display_name: display_name.to_string(),
        plugin_namespace: None,
        description: None,
        has_skills: true,
        mcp_server_names: Vec::new(),
        app_connector_ids: Vec::new(),
    }
}

#[test]
fn collect_explicit_app_ids_from_linked_text_mentions() {
    let input = vec![text_input("use [$calendar](app://calendar)")];

    let app_ids = collect_explicit_app_ids(&input);

    assert_eq!(app_ids, HashSet::from(["calendar".to_string()]));
}

#[test]
fn collect_explicit_app_ids_dedupes_structured_and_linked_mentions() {
    let input = vec![
        text_input("use [$calendar](app://calendar)"),
        UserInput::mention("calendar", "app://calendar"),
    ];

    let app_ids = collect_explicit_app_ids(&input);

    assert_eq!(app_ids, HashSet::from(["calendar".to_string()]));
}

#[test]
fn collect_explicit_app_ids_ignores_non_app_paths() {
    let input = vec![
        text_input(
            "use [$docs](mcp://docs) and [$skill](skill://team/skill) and [$file](/tmp/file.txt)",
        ),
        UserInput::mention("docs", "mcp://docs"),
        UserInput::mention("skill", "skill://team/skill"),
        UserInput::mention("file", "/tmp/file.txt"),
    ];

    let app_ids = collect_explicit_app_ids(&input);

    assert_eq!(app_ids, HashSet::<String>::new());
}

#[test]
fn collect_explicit_plugin_mentions_from_structured_paths() {
    let expected = plugin("sample@test", "Sample");
    let plugins = vec![
        expected.clone(),
        plugin("sample@other-marketplace", "Sample"),
        plugin("other@test", "alias"),
    ];

    let mentioned = collect_explicit_plugin_mentions(
        &[UserInput::mention("alias", "plugin://sample@test")],
        &plugins,
    );

    assert_eq!(mentioned, vec![expected]);
}

#[test]
fn collect_explicit_plugin_mentions_from_linked_text_mentions() {
    let expected = plugin("sample@test", "Sample");
    let plugins = vec![
        expected.clone(),
        plugin("sample@other-marketplace", "Sample"),
        plugin("other@test", "alias"),
    ];

    let mentioned = collect_explicit_plugin_mentions(
        &[text_input("use [@alias](plugin://sample@test)")],
        &plugins,
    );

    assert_eq!(mentioned, vec![expected]);
}

#[test]
fn collect_explicit_plugin_mentions_dedupes_structured_and_linked_mentions() {
    let plugins = vec![
        plugin("sample@test", "sample"),
        plugin("other@test", "other"),
    ];

    let mentioned = collect_explicit_plugin_mentions(
        &[
            text_input("use [@sample](plugin://sample@test)"),
            UserInput::mention("sample", "plugin://sample@test"),
        ],
        &plugins,
    );

    assert_eq!(mentioned, vec![plugin("sample@test", "sample")]);
}

#[test]
fn collect_explicit_plugin_mentions_ignores_non_plugin_paths() {
    let plugins = vec![plugin("sample@test", "sample")];

    let mentioned = collect_explicit_plugin_mentions(
        &[text_input(
            "use [$app](app://calendar) and [$skill](skill://team/skill) and [$file](/tmp/file.txt)",
        )],
        &plugins,
    );

    assert_eq!(mentioned, Vec::<PluginCapabilitySummary>::new());
}

#[test]
fn collect_explicit_plugin_mentions_ignores_dollar_linked_plugin_mentions() {
    let plugins = vec![plugin("sample@test", "sample")];

    let mentioned = collect_explicit_plugin_mentions(
        &[text_input("use [$sample](plugin://sample@test)")],
        &plugins,
    );

    assert_eq!(mentioned, Vec::<PluginCapabilitySummary>::new());
}

#[test]
fn collect_explicit_plugin_ids_preserves_opaque_ids() {
    let input = [
        text_input("use [@same](plugin://selected-one)"),
        UserInput::mention("same", "plugin://selected-two"),
    ];

    assert_eq!(
        collect_explicit_plugin_ids(&input),
        HashSet::from(["selected-one".to_string(), "selected-two".to_string()]),
    );
}
