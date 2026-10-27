// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Product configuration source and composition contracts through public entry points.

use std::cmp::Ordering;

use ra_config::{
    ConfigLayerSource, ShellEnvironmentPolicyFilterRepresentation as FilterRepresentation,
    build_cli_overrides_layer, format_config_layer_source, is_structured_feature_path,
    merge_toml_values, shell_environment_filter_entry, version_for_toml,
};
use ra_paths::AbsolutePathBuf;
use toml::Value;

fn path(name: &str) -> AbsolutePathBuf {
    AbsolutePathBuf::from_absolute_path(std::env::temp_dir().join(name)).unwrap()
}

fn parse(value: &str) -> Value {
    toml::from_str(value).unwrap()
}

#[test]
fn product_sources_preserve_the_full_precedence_order() {
    let sources = [
        (
            ConfigLayerSource::PackagedDefaults {
                file: path("defaults.toml"),
            },
            -10,
        ),
        (
            ConfigLayerSource::Mdm {
                domain: "product".into(),
                key: "config".into(),
            },
            0,
        ),
        (
            ConfigLayerSource::System {
                file: path("system.toml"),
            },
            10,
        ),
        (
            ConfigLayerSource::EnterpriseManaged {
                id: "fragment".into(),
                name: "Policy".into(),
            },
            15,
        ),
        (
            ConfigLayerSource::User {
                file: path("config.toml"),
                profile: None,
            },
            20,
        ),
        (
            ConfigLayerSource::User {
                file: path("work.config.toml"),
                profile: Some("work".into()),
            },
            21,
        ),
        (
            ConfigLayerSource::Project {
                dot_codex_folder: path("project/.rusty-agent"),
            },
            25,
        ),
        (ConfigLayerSource::SessionFlags, 30),
        (
            ConfigLayerSource::LegacyManagedConfigTomlFromFile {
                file: path("managed_config.toml"),
            },
            40,
        ),
        (ConfigLayerSource::LegacyManagedConfigTomlFromMdm, 50),
    ];
    for (source, expected) in &sources {
        assert_eq!(source.precedence(), *expected);
    }
    for pair in sources.windows(2) {
        assert!(pair[0].0 < pair[1].0);
    }
    assert!(sources[8].0 > ConfigLayerSource::SessionFlags);
}

#[test]
fn same_precedence_does_not_erase_source_identity() {
    let first = ConfigLayerSource::Project {
        dot_codex_folder: path("project/.rusty-agent"),
    };
    let second = ConfigLayerSource::Project {
        dot_codex_folder: path("project/nested/.rusty-agent"),
    };
    assert_ne!(first, second);
    assert_eq!(first.partial_cmp(&second), Some(Ordering::Equal));
    let base_user = ConfigLayerSource::User {
        file: path("config.toml"),
        profile: None,
    };
    let selected = ConfigLayerSource::User {
        file: path("work.config.toml"),
        profile: Some("work".into()),
    };
    assert!(base_user < selected);
}

#[test]
fn source_display_preserves_paths_and_distinct_managed_sources() {
    let file = path("config.toml");
    let folder = path("project/.rusty-agent");
    for (source, expected) in [
        (
            ConfigLayerSource::PackagedDefaults { file: file.clone() },
            format!("packaged defaults ({})", file.display()),
        ),
        (
            ConfigLayerSource::Mdm {
                domain: "product.domain".into(),
                key: "config".into(),
            },
            "MDM (product.domain:config)".into(),
        ),
        (
            ConfigLayerSource::System { file: file.clone() },
            format!("system ({})", file.display()),
        ),
        (
            ConfigLayerSource::EnterpriseManaged {
                id: "id-1".into(),
                name: "Policy".into(),
            },
            "enterprise-managed (Policy, id-1)".into(),
        ),
        (
            ConfigLayerSource::User {
                file: file.clone(),
                profile: Some("work".into()),
            },
            format!("user ({})", file.display()),
        ),
        (
            ConfigLayerSource::Project {
                dot_codex_folder: folder.clone(),
            },
            format!("project ({}/config.toml)", folder.display()),
        ),
        (ConfigLayerSource::SessionFlags, "session-flags".into()),
        (
            ConfigLayerSource::LegacyManagedConfigTomlFromFile { file: file.clone() },
            format!("legacy managed_config.toml ({})", file.display()),
        ),
        (
            ConfigLayerSource::LegacyManagedConfigTomlFromMdm,
            "legacy managed_config.toml (MDM)".into(),
        ),
    ] {
        assert_eq!(format_config_layer_source(&source, "config.toml"), expected);
    }
}

#[test]
fn fingerprint_preserves_nested_key_order_independence_and_existing_hash() {
    for input in [
        "rows = [{ z = 2, a = 1 }, { z = 4, a = 3 }]\n[nested]\nz = 'text'\na = true\n",
        "rows = [{ a = 1, z = 2 }, { a = 3, z = 4 }]\n[nested]\na = true\nz = 'text'\n",
    ] {
        assert_eq!(
            version_for_toml(&parse(input)),
            "sha256:a5e4c8fbdbc75d185b0f2b5291bfb2de10e8c523d6c89248876fd78a97c83176"
        );
    }
}

#[test]
fn fingerprint_is_sensitive_to_array_order_and_value_type() {
    assert_ne!(
        version_for_toml(&parse("items = [1, 2]")),
        version_for_toml(&parse("items = [2, 1]"))
    );
    assert_ne!(
        version_for_toml(&parse("enabled = true")),
        version_for_toml(&parse("enabled = 'true'"))
    );
    assert_eq!(
        version_for_toml(&parse("")),
        "sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a"
    );
}

#[test]
fn ordinary_toml_tables_merge_recursively_and_arrays_replace() {
    let mut base =
        parse("[sample]\nitems = ['first', 'shared']\nkeep = true\n[sample.nested]\nfirst = 1");
    let overlay = parse("[sample]\nitems = ['second']\n[sample.nested]\nsecond = 2");
    let before = overlay.clone();
    merge_toml_values(&mut base, &overlay);
    assert_eq!(
        base,
        parse("[sample]\nitems = ['second']\nkeep = true\n[sample.nested]\nfirst = 1\nsecond = 2")
    );
    assert_eq!(overlay, before);
}

#[test]
fn ordinary_scalar_and_table_conflicts_replace_the_lower_value() {
    for (base, overlay) in [
        ("sample = false", "[sample]\nvalue = 1"),
        ("[sample]\nvalue = 1", "sample = false"),
    ] {
        let mut base = parse(base);
        let overlay = parse(overlay);
        merge_toml_values(&mut base, &overlay);
        assert_eq!(base, overlay);
    }
}

#[test]
fn ordered_session_overrides_rebuild_scalar_ancestors_and_preserve_siblings() {
    let overrides = vec![
        ("sample".into(), Value::Boolean(true)),
        ("sample.nested.value".into(), Value::Integer(1)),
        ("sample.other".into(), Value::String("keep".into())),
        ("sample.nested.value".into(), Value::Integer(2)),
    ];
    assert_eq!(
        build_cli_overrides_layer(&overrides),
        parse("[sample]\nother = 'keep'\n[sample.nested]\nvalue = 2")
    );
    assert_eq!(build_cli_overrides_layer(&[]), parse(""));
}

#[test]
fn structured_feature_recognition_is_limited_to_original_paths() {
    for feature in ["code_mode", "multi_agent_v2", "network_proxy", "sleep_tool"] {
        assert!(is_structured_feature_path(&["features", feature]));
        assert!(is_structured_feature_path(&[
            "profiles", "work", "features", feature
        ]));
        assert!(!is_structured_feature_path(&[
            "desktop", "features", feature
        ]));
        assert!(!is_structured_feature_path(&[
            "features", feature, "enabled"
        ]));
    }
    assert!(!is_structured_feature_path(&["features", "plugins"]));
    assert!(!is_structured_feature_path::<&str>(&[]));
}

#[test]
fn shell_filter_representation_detection_preserves_explicit_empty_fields() {
    assert_eq!(
        FilterRepresentation::from_policy(&parse("exclude = []")),
        Some(FilterRepresentation::Legacy)
    );
    assert_eq!(
        FilterRepresentation::from_policy(&parse("include_only = []")),
        Some(FilterRepresentation::Legacy)
    );
    assert_eq!(
        FilterRepresentation::from_policy(&parse("[filters]")),
        Some(FilterRepresentation::Filters)
    );
    assert_eq!(
        FilterRepresentation::from_policy(&parse("exclude = []\n[filters]")),
        Some(FilterRepresentation::Filters)
    );
    assert_eq!(
        FilterRepresentation::from_policy(&Value::Boolean(false)),
        None
    );
    assert_eq!(
        FilterRepresentation::from_policy(&parse("inherit = 'all'")),
        None
    );
    assert_eq!(
        FilterRepresentation::Filters.displaced_fields(),
        &["exclude", "include_only"]
    );
    assert_eq!(
        FilterRepresentation::Legacy.displaced_fields(),
        &["filters"]
    );
}

#[test]
fn shell_filter_edit_detection_uses_the_original_path_contract() {
    let path = |parts: &[&str]| {
        parts
            .iter()
            .map(|part| (*part).to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        FilterRepresentation::from_path(&path(&["shell_environment_policy", "filters", "PATH"])),
        Some(FilterRepresentation::Filters)
    );
    assert_eq!(
        FilterRepresentation::from_path(&path(&["shell_environment_policy", "exclude"])),
        Some(FilterRepresentation::Legacy)
    );
    assert_eq!(
        FilterRepresentation::from_path(&path(&["other", "filters"])),
        None
    );
    assert_eq!(
        FilterRepresentation::from_edit(&path(&["shell_environment_policy"]), &parse("[filters]")),
        Some(FilterRepresentation::Filters)
    );
    assert_eq!(
        FilterRepresentation::from_edit(
            &path(&["shell_environment_policy", "include_only"]),
            &Value::Array(vec![])
        ),
        Some(FilterRepresentation::Legacy)
    );
}

#[test]
fn shell_filter_lookup_matches_unicode_case_and_retains_original_key() {
    let root =
        parse("[shell_environment_policy.filters]\n'СЕКРЕТ_*' = 'exclude'\nPATH = 'include'");
    let path = vec![
        "shell_environment_policy".into(),
        "filters".into(),
        "секрет_*".into(),
    ];
    let (key, value) = shell_environment_filter_entry(&root, &path).unwrap();
    assert_eq!(key, "СЕКРЕТ_*");
    assert_eq!(value.as_str(), Some("exclude"));
    assert!(
        shell_environment_filter_entry(
            &root,
            &["shell_environment_policy".into(), "filters".into()]
        )
        .is_none()
    );
    assert!(
        shell_environment_filter_entry(&root, &["other".into(), "filters".into(), "PATH".into()])
            .is_none()
    );
}

#[test]
fn network_domain_merge_preserves_upstream_host_normalization() {
    for (input, expected) in [
        ("  ExAmPlE.CoM  ", "example.com"),
        ("example.com:1234", "example.com"),
        ("2001:db8::1", "2001:db8::1"),
        ("ExAmPlE.CoM.", "example.com"),
        ("example.com.:443", "example.com"),
        ("[::1]", "::1"),
        ("[::1]:443", "::1"),
        ("fe80::1%lo0", "fe80::1%lo0"),
        ("[fe80::1%lo0]", "fe80::1%lo0"),
        ("[fe80::1%25lo0]", "fe80::1%lo0"),
    ] {
        for scope in [
            "permissions.dev.network.domains",
            "application.network.domains",
        ] {
            let mut base = parse(&format!("[{scope}]\n{expected:?} = 'deny'"));
            let overlay = parse(&format!("[{scope}]\n{input:?} = 'allow'"));
            merge_toml_values(&mut base, &overlay);
            assert_eq!(
                base,
                parse(&format!("[{scope}]\n{expected:?} = 'allow'")),
                "{scope}: {input}"
            );
        }
    }
}

#[test]
fn network_domain_collisions_use_the_last_key_written_in_each_table() {
    for scope in [
        "permissions.dev.network.domains",
        "application.network.domains",
    ] {
        for (first, first_policy, second, second_policy) in [
            ("example.com", "deny", "Example.COM", "allow"),
            ("Example.COM", "allow", "example.com", "deny"),
        ] {
            let collisions = parse(&format!(
                "[{scope}]\n{first:?} = {first_policy:?}\n{second:?} = {second_policy:?}"
            ));
            let sibling = parse(&format!("[{scope}]\n'other.example' = 'deny'"));
            let expected = parse(&format!(
                "[{scope}]\n'example.com' = {second_policy:?}\n'other.example' = 'deny'"
            ));
            for (mut base, overlay) in
                [(collisions.clone(), sibling.clone()), (sibling, collisions)]
            {
                merge_toml_values(&mut base, &overlay);
                assert_eq!(base, expected, "{scope}: {first} before {second}");
            }
        }
    }
}

#[test]
fn shell_filter_collisions_use_the_last_key_written_in_each_table() {
    for (first, first_policy, second, second_policy) in [
        ("path", "exclude", "PATH", "include"),
        ("PATH", "include", "path", "exclude"),
    ] {
        let collisions = parse(&format!(
            "[shell_environment_policy.filters]\n{first:?} = {first_policy:?}\n{second:?} = {second_policy:?}"
        ));
        let sibling = parse("[shell_environment_policy.filters]\nOTHER = 'exclude'");
        let expected = parse(&format!(
            "[shell_environment_policy.filters]\npath = {second_policy:?}\nother = 'exclude'"
        ));
        for (mut base, overlay) in [(collisions.clone(), sibling.clone()), (sibling, collisions)] {
            merge_toml_values(&mut base, &overlay);
            assert_eq!(base, expected, "{first} before {second}");
        }
    }
}

#[test]
fn shell_filter_lookup_returns_the_first_key_written_that_matches() {
    let query = vec![
        "shell_environment_policy".into(),
        "filters".into(),
        "PaTh".into(),
    ];
    for (first, first_policy, second, second_policy) in [
        ("path", "exclude", "PATH", "include"),
        ("PATH", "include", "path", "exclude"),
    ] {
        let root = parse(&format!(
            "[shell_environment_policy.filters]\n{first:?} = {first_policy:?}\n{second:?} = {second_policy:?}"
        ));
        let (key, value) = shell_environment_filter_entry(&root, &query).unwrap();
        assert_eq!(key, first);
        assert_eq!(value.as_str(), Some(first_policy));
    }
}
