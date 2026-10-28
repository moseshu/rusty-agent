// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.

use dirs::home_dir;
use ra_config::test_find_ra_home_from_env as find_ra_home_from_env;
use ra_paths::AbsolutePathBuf;
use std::fs;
use std::io::ErrorKind;
use tempfile::TempDir;

#[test]
fn find_ra_home_env_missing_path_is_fatal() {
    let temp_home = TempDir::new().expect("temp home");
    let missing = temp_home.path().join("missing-codex-home");
    let missing_str = missing
        .to_str()
        .expect("missing codex home path should be valid utf-8");

    let err = find_ra_home_from_env(Some(missing_str)).expect_err("missing RA_HOME");
    assert_eq!(err.kind(), ErrorKind::NotFound);
    assert!(
        err.to_string().contains("RA_HOME"),
        "unexpected error: {err}"
    );
}

#[test]
fn find_ra_home_env_file_path_is_fatal() {
    let temp_home = TempDir::new().expect("temp home");
    let file_path = temp_home.path().join("codex-home.txt");
    fs::write(&file_path, "not a directory").expect("write temp file");
    let file_str = file_path
        .to_str()
        .expect("file codex home path should be valid utf-8");

    let err = find_ra_home_from_env(Some(file_str)).expect_err("file RA_HOME");
    assert_eq!(err.kind(), ErrorKind::InvalidInput);
    assert!(
        err.to_string().contains("not a directory"),
        "unexpected error: {err}"
    );
}

#[test]
fn find_ra_home_env_valid_directory_canonicalizes() {
    let temp_home = TempDir::new().expect("temp home");
    let temp_str = temp_home
        .path()
        .to_str()
        .expect("temp codex home path should be valid utf-8");

    let resolved = find_ra_home_from_env(Some(temp_str)).expect("valid RA_HOME");
    let expected = temp_home
        .path()
        .canonicalize()
        .expect("canonicalize temp home");
    let expected = AbsolutePathBuf::from_absolute_path(expected).expect("absolute home");
    assert_eq!(resolved, expected);
}

#[test]
fn find_ra_home_without_env_uses_default_home_dir() {
    let resolved = find_ra_home_from_env(/*codex_home_env*/ None).expect("default RA_HOME");
    let mut expected = home_dir().expect("home dir");
    expected.push(".rusty-agent");
    let expected = AbsolutePathBuf::from_absolute_path(expected).expect("absolute home");
    assert_eq!(resolved, expected);
}
