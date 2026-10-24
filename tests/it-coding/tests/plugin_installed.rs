// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Version selection exercises the read-only common helper without constructing a Store.

use ra_coding::plugins::{
    AbsolutePathBuf,
    installed::{active_plugin_version, compare_plugin_versions, validate_plugin_version_segment},
};
use std::{cmp::Ordering, fs};

#[test]
fn active_version_reads_directory_name_and_prefers_local() {
    let temp = tempfile::tempdir().unwrap();
    let root = AbsolutePathBuf::try_from(temp.path()).unwrap();
    assert_eq!(active_plugin_version(&root), None);
    for version in ["1.2.0", "1.10.0", "local"] {
        fs::create_dir(temp.path().join(version)).unwrap();
    }
    fs::write(temp.path().join("local/plugin.json"), "{broken").unwrap();
    assert_eq!(active_plugin_version(&root).as_deref(), Some("local"));
    assert_eq!(
        fs::read_to_string(temp.path().join("local/plugin.json")).unwrap(),
        "{broken"
    );
    fs::remove_dir_all(temp.path().join("local")).unwrap();
    assert_eq!(active_plugin_version(&root).as_deref(), Some("1.10.0"));
}

#[test]
fn version_order_uses_semver_only_when_both_versions_parse() {
    assert_eq!(compare_plugin_versions("1.9.0", "1.10.0"), Ordering::Less);
    assert_eq!(compare_plugin_versions("1.9", "1.10"), Ordering::Greater);
    assert_eq!(
        compare_plugin_versions("release-a", "release-z"),
        Ordering::Less
    );
    assert_eq!(
        compare_plugin_versions("1.0.0-alpha", "1.0.0"),
        Ordering::Less
    );
    assert_eq!(compare_plugin_versions("local", "local"), Ordering::Equal);
}

#[test]
fn version_segments_preserve_their_own_character_contract() {
    for version in ["local", "1.0.0-beta+build.7", ".hidden", "a_b-c"] {
        validate_plugin_version_segment(version).unwrap();
    }
    for version in ["", ".", "..", "a/b", "a\\b", "a@b", "a b", "\u{00e9}"] {
        assert!(
            validate_plugin_version_segment(version).is_err(),
            "{version}"
        );
    }
    assert_eq!(
        validate_plugin_version_segment("").unwrap_err(),
        "invalid plugin version: must not be empty"
    );
    assert_eq!(
        validate_plugin_version_segment("..").unwrap_err(),
        "invalid plugin version: path traversal is not allowed"
    );
}

#[test]
fn invalid_versions_and_regular_files_do_not_become_active() {
    let temp = tempfile::tempdir().unwrap();
    let root = AbsolutePathBuf::try_from(temp.path()).unwrap();
    fs::write(temp.path().join("local"), "not a directory").unwrap();
    fs::create_dir(temp.path().join("bad version")).unwrap();
    assert_eq!(active_plugin_version(&root), None);
    fs::create_dir(temp.path().join("release-a")).unwrap();
    fs::create_dir(temp.path().join("release-z")).unwrap();
    assert_eq!(active_plugin_version(&root).as_deref(), Some("release-z"));
    let missing = AbsolutePathBuf::try_from(temp.path().join("missing")).unwrap();
    assert_eq!(active_plugin_version(&missing), None);
    assert!(!missing.as_path().exists());
}

#[cfg(unix)]
#[test]
fn symlinked_versions_are_skipped() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    let target = tempfile::tempdir().unwrap();
    symlink(target.path(), temp.path().join("local")).unwrap();
    assert_eq!(
        active_plugin_version(&AbsolutePathBuf::try_from(temp.path()).unwrap()),
        None
    );
}

#[cfg(target_os = "linux")]
#[test]
fn non_utf8_versions_are_skipped() {
    use std::os::unix::ffi::OsStringExt;
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir(temp.path().join(std::ffi::OsString::from_vec(vec![0xff]))).unwrap();
    assert_eq!(
        active_plugin_version(&AbsolutePathBuf::try_from(temp.path()).unwrap()),
        None
    );
}
