// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Exercise managed preference trust checks with injected reads and in-memory values.

#![cfg(target_os = "macos")]
use ra_exec::platform_config::test_load_managed_preference_with as load_managed_preference_with;
const MANAGED_PREFERENCES_APPLICATION_ID: &str = "io.github.moseshu.rusty-agent";
const MANAGED_PREFERENCES_CONFIG_KEY: &str = "config_toml_base64";
const MANAGED_PREFERENCES_REQUIREMENTS_KEY: &str = "requirements_toml_base64";
use base64::Engine;
use base64::prelude::BASE64_STANDARD;
use core_foundation::array::CFArray;
use core_foundation::base::TCFType;
use core_foundation::boolean::CFBoolean;
use core_foundation::data::CFData;
use core_foundation::dictionary::CFDictionary;
use core_foundation::number::CFNumber;
use core_foundation::string::CFString;
use std::cell::Cell;
use std::io;

#[test]
fn unforced_preferences_are_ignored_without_reading() {
    for key in [
        MANAGED_PREFERENCES_CONFIG_KEY,
        MANAGED_PREFERENCES_REQUIREMENTS_KEY,
    ] {
        assert_eq!(
            load_managed_preference_with(
                MANAGED_PREFERENCES_APPLICATION_ID,
                key,
                || false,
                || panic!("unforced preference must not be read"),
            )
            .expect("ignore unforced preference"),
            None
        );
    }
}

#[test]
fn forced_preferences_without_a_value_are_absent() {
    for key in [
        MANAGED_PREFERENCES_CONFIG_KEY,
        MANAGED_PREFERENCES_REQUIREMENTS_KEY,
    ] {
        assert_eq!(
            load_managed_preference_with(MANAGED_PREFERENCES_APPLICATION_ID, key, || true, || None)
                .expect("load absent forced preference"),
            None
        );
    }
}

#[test]
fn preferences_that_become_unforced_during_read_are_ignored() {
    for key in [
        MANAGED_PREFERENCES_CONFIG_KEY,
        MANAGED_PREFERENCES_REQUIREMENTS_KEY,
    ] {
        for value in [
            CFString::new("ordinary user default").as_CFType(),
            CFBoolean::true_value().as_CFType(),
        ] {
            let forced = Cell::new(/*value*/ true);
            assert_eq!(
                load_managed_preference_with(
                    MANAGED_PREFERENCES_APPLICATION_ID,
                    key,
                    || forced.get(),
                    || {
                        forced.set(/*val*/ false);
                        Some(value)
                    },
                )
                .expect("ignore preference that became unforced during the read"),
                None
            );
        }
    }
}

#[test]
fn forced_string_preferences_preserve_contents() {
    let encoded = BASE64_STANDARD.encode("sandbox_mode = 'read-only'");
    for key in [
        MANAGED_PREFERENCES_CONFIG_KEY,
        MANAGED_PREFERENCES_REQUIREMENTS_KEY,
    ] {
        for contents in ["", "Préférence gérée ✓", &encoded] {
            assert_eq!(
                load_managed_preference_with(
                    MANAGED_PREFERENCES_APPLICATION_ID,
                    key,
                    || true,
                    || Some(CFString::new(contents).as_CFType()),
                )
                .expect("load string preference"),
                Some(contents.to_owned())
            );
        }
    }
}

#[test]
fn forced_non_string_preferences_return_redacted_errors() {
    let private_contents = CFString::new("private preference contents");
    let values = [
        ("boolean", CFBoolean::true_value().as_CFType()),
        ("number", CFNumber::from(/*value*/ 42).as_CFType()),
        (
            "data",
            CFData::from_buffer(b"private preference contents").as_CFType(),
        ),
        (
            "array",
            CFArray::from_CFTypes(std::slice::from_ref(&private_contents)).as_CFType(),
        ),
        (
            "dictionary",
            CFDictionary::from_CFType_pairs(&[(CFString::new("secret"), private_contents)])
                .as_CFType(),
        ),
    ];
    for key in [
        MANAGED_PREFERENCES_CONFIG_KEY,
        MANAGED_PREFERENCES_REQUIREMENTS_KEY,
    ] {
        for (type_name, value) in &values {
            let error = load_managed_preference_with(
                MANAGED_PREFERENCES_APPLICATION_ID,
                key,
                || true,
                || Some(value.clone()),
            )
            .expect_err("reject non-string preference");
            assert_eq!(
                (error.kind(), error.to_string()),
                (
                    io::ErrorKind::InvalidData,
                    format!(
                        "Managed preference {MANAGED_PREFERENCES_APPLICATION_ID}:{key} must be a string"
                    ),
                ),
                "unexpected diagnostic for {type_name} preference {key}"
            );
        }
    }
}
