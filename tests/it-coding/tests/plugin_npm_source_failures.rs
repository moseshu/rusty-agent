// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! npm command, archive-selection, metadata, limits, and failure cleanup contracts.

#![cfg(unix)]

use flate2::{Compression, write::GzEncoder};
use ra_coding::plugins::materialize::test_api::{materialize_npm_with_command, read_npm_archive};
use std::{
    fs,
    io::{Cursor, Write},
    path::{Path, PathBuf},
};

fn executable(path: &Path, contents: &str) {
    use std::os::unix::fs::PermissionsExt;
    fs::write(path, contents).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn shell_path(path: &Path) -> String {
    format!("'{}'", path.to_str().unwrap().replace('\'', "'\\''"))
}

fn package_archive(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut tar = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::default()));
    for (name, bytes) in entries {
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, name, Cursor::new(bytes))
            .unwrap();
    }
    tar.into_inner().unwrap().finish().unwrap()
}

fn fake_npm(root: &Path, archive: &[u8], action: &str) -> PathBuf {
    fs::create_dir_all(root).unwrap();
    fs::write(root.join("fixture.tgz"), archive).unwrap();
    let bin = root.join("npm");
    executable(
        &bin,
        &format!(
            r#"#!/bin/sh
fixture={}
{action}
"#,
            shell_path(&root.join("fixture.tgz"))
        ),
    );
    bin
}

fn assert_empty_staging(home: &Path) {
    let staging = home.join("plugins/.marketplace-plugin-source-staging");
    assert_eq!(fs::read_dir(staging).unwrap().count(), 0);
}

#[test]
fn npm_spawn_and_nonzero_failures_retain_diagnostics_and_remove_temporary_sources() {
    let home = tempfile::tempdir().unwrap();
    let missing = home.path().join("missing-npm");
    let err = materialize_npm_with_command(home.path(), "demo", None, None, missing.as_os_str())
        .unwrap_err();
    assert!(err.starts_with("failed to run npm pack:"));
    assert_empty_staging(home.path());
    let bin = fake_npm(
        home.path(),
        b"",
        "printf ' pack output \\n'; printf ' pack error \\n' >&2; exit 7",
    );
    let err =
        materialize_npm_with_command(home.path(), "demo", None, None, bin.as_os_str()).unwrap_err();
    assert!(err.starts_with("npm pack failed with status"));
    assert!(err.ends_with("\nstdout:\npack output\nstderr:\npack error"));
    assert_empty_staging(home.path());
}

#[test]
fn npm_requires_exactly_one_regular_tgz_file() {
    let home = tempfile::tempdir().unwrap();
    let archive = package_archive(&[("package/package.json", br#"{"name":"demo"}"#)]);
    for (action, count) in [
        ("mkdir ignored.tgz; cp \"$fixture\" ignored.zip", 0),
        ("ln -s \"$fixture\" symlink.tgz", 0),
        ("cp \"$fixture\" first.tgz; cp \"$fixture\" second.tgz", 2),
    ] {
        let bin = fake_npm(home.path(), &archive, action);
        let err = materialize_npm_with_command(home.path(), "demo", None, None, bin.as_os_str())
            .unwrap_err();
        assert_eq!(
            err,
            format!("npm pack completed with {count} package archives; expected exactly one")
        );
        assert_empty_staging(home.path());
    }
}

#[test]
fn npm_requires_package_root_metadata_and_exact_requested_name() {
    let home = tempfile::tempdir().unwrap();
    for (entries, expected) in [
        (
            vec![("other/package.json", b"{}".as_slice())],
            "npm pack completed without creating plugin package directory",
        ),
        (
            vec![("package/file", b"data".as_slice())],
            "failed to read npm plugin package metadata",
        ),
        (
            vec![("package/package.json", b"{".as_slice())],
            "failed to parse npm plugin package metadata",
        ),
        (
            vec![("package/package.json", br#"{"name":"other"}"#.as_slice())],
            "npm plugin package name 'other' does not match requested package 'demo'",
        ),
    ] {
        let bin = fake_npm(
            home.path(),
            &package_archive(&entries),
            "cp \"$fixture\" package.tgz",
        );
        let err = materialize_npm_with_command(home.path(), "demo", None, None, bin.as_os_str())
            .unwrap_err();
        assert!(err.starts_with(expected), "{err}");
        assert_empty_staging(home.path());
    }
}

#[test]
fn npm_defaults_preserve_exact_arguments_and_return_owned_extraction() {
    let home = tempfile::tempdir().unwrap();
    let archive = package_archive(&[(
        "package/package.json",
        br#"{"name":"demo","version":"99.0.0"}"#,
    )]);
    let bin = fake_npm(
        home.path(),
        &archive,
        "cp \"$fixture\" package.tgz; printf '%s\\n' \"$@\" > args.txt",
    );
    let (root, owner) =
        materialize_npm_with_command(home.path(), "demo", None, None, bin.as_os_str()).unwrap();
    let args = fs::read_to_string(owner.path().join("args.txt")).unwrap();
    assert_eq!(
        args.lines().collect::<Vec<_>>(),
        [
            "pack",
            "--ignore-scripts",
            "--pack-destination",
            owner.path().to_str().unwrap(),
            "--",
            "demo"
        ]
    );
    assert!(root.as_path().join("package.json").is_file());
    assert_eq!(root.as_path(), owner.path().join("extracted/package"));
    let temporary = owner.path().to_path_buf();
    drop(owner);
    assert!(!temporary.exists());
}

#[test]
fn npm_rejects_oversized_archives_before_reading_them() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("large.tgz");
    fs::File::create(&path)
        .unwrap()
        .set_len(50 * 1024 * 1024 + 1)
        .unwrap();
    assert_eq!(
        read_npm_archive(&path).unwrap_err(),
        "npm package archive is 52428801 bytes, exceeding maximum size of 52428800 bytes"
    );
}

#[test]
fn npm_enforces_the_extracted_limit_before_reading_a_large_member_body() {
    let home = tempfile::tempdir().unwrap();
    let mut header = tar::Header::new_gnu();
    header.set_path("package/large").unwrap();
    header.set_mode(0o644);
    header.set_size(250 * 1024 * 1024 + 1);
    header.set_cksum();
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(header.as_bytes()).unwrap();
    encoder.write_all(&[0; 1024]).unwrap();
    let bytes = encoder.finish().unwrap();
    let bin = fake_npm(home.path(), &bytes, "cp \"$fixture\" package.tgz");
    let err =
        materialize_npm_with_command(home.path(), "demo", None, None, bin.as_os_str()).unwrap_err();
    assert_eq!(
        err,
        "failed to extract npm plugin package: plugin bundle extracted size would be 262144001 bytes, exceeding maximum total size of 262144000 bytes"
    );
    assert_empty_staging(home.path());
}

#[test]
fn npm_staging_failure_preserves_the_existing_file() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("file");
    fs::write(&home, b"kept").unwrap();
    let err =
        materialize_npm_with_command(&home, "demo", None, None, Path::new("unused").as_os_str())
            .unwrap_err();
    assert!(err.starts_with("failed to create marketplace plugin source staging directory"));
    assert_eq!(fs::read(home).unwrap(), b"kept");
}
