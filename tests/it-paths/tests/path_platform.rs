// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Coverage for platform metadata and platform-derived path conventions.

use ra_paths::*;

#[test]
fn platform_metadata_preserves_missing_and_unrecognized_values() {
    for (metadata, expected) in [
        (Some("linux"), Platform::Linux),
        (Some("macos"), Platform::Macos),
        (Some("windows"), Platform::Windows),
        (None, Platform::Unknown),
        (Some("freebsd"), Platform::Unknown),
        (Some("Windows"), Platform::Unknown),
        (Some(""), Platform::Unknown),
    ] {
        assert_eq!(
            Platform::from_platform_os(metadata),
            expected,
            "{metadata:?}"
        );
    }
}

#[test]
fn path_convention_is_derived_from_platform() {
    for (platform, expected) in [
        (Platform::Linux, Some(PathConvention::Posix)),
        (Platform::Macos, Some(PathConvention::Posix)),
        (Platform::Windows, Some(PathConvention::Windows)),
        (Platform::Unknown, None),
    ] {
        assert_eq!(platform.path_convention(), expected, "{platform:?}");
    }
}

#[test]
fn native_platform_matches_current_process_metadata() {
    assert_eq!(
        Platform::native(),
        Platform::from_platform_os(Some(std::env::consts::OS)),
    );
}
