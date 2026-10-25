// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Archive limits, rejected members, and partial-extraction behavior.

use flate2::{Compression, write::GzEncoder};
use ra_coding::plugins::materialize::test_api::{
    PluginBundlePackError, PluginBundleUnpackError, enforce_size, pack, unpack,
};
use std::{fs, io::Cursor};

fn archive(entries: &[(&str, tar::EntryType, &[u8])]) -> Vec<u8> {
    let mut archive = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::default()));
    for (path, kind, bytes) in entries {
        let mut header = tar::Header::new_gnu();
        header.set_mode(0o755);
        header.set_entry_type(*kind);
        header.set_size(bytes.len() as u64);
        if kind.is_symlink() || kind.is_hard_link() {
            header.set_link_name("target").unwrap();
        }
        // Raw names exercise checks that tar::Builder::append_data rejects before serialization.
        assert!(path.len() < 100);
        header.as_mut_bytes()[..path.len()].copy_from_slice(path.as_bytes());
        header.set_cksum();
        archive.append(&header, Cursor::new(bytes)).unwrap();
    }
    archive.into_inner().unwrap().finish().unwrap()
}

#[test]
fn traversal_absolute_and_root_equivalent_names_are_rejected() {
    let destination = tempfile::tempdir().unwrap();
    for name in ["../escape", "ok/../../escape", "/escape", ".", "./"] {
        let bytes = archive(&[(name, tar::EntryType::Regular, b"bad")]);
        assert!(
            matches!(
                unpack(&bytes, destination.path(), 100),
                Err(PluginBundleUnpackError::InvalidBundle(_))
            ),
            "{name}"
        );
    }
    assert_eq!(fs::read_dir(destination.path()).unwrap().count(), 0);
}

#[test]
fn links_and_special_members_are_rejected() {
    let destination = tempfile::tempdir().unwrap();
    for kind in [
        tar::EntryType::Symlink,
        tar::EntryType::Link,
        tar::EntryType::Fifo,
        tar::EntryType::Char,
        tar::EntryType::Block,
    ] {
        let bytes = archive(&[("member", kind, b"")]);
        let err = unpack(&bytes, destination.path(), 100).unwrap_err();
        assert!(matches!(err, PluginBundleUnpackError::InvalidBundle(_)));
        assert!(!destination.path().join("member").exists());
    }
}

#[test]
fn extracted_limit_is_inclusive_and_counts_duplicate_members() {
    let bytes = archive(&[
        ("dir", tar::EntryType::Directory, b""),
        ("dir/item", tar::EntryType::Regular, b"abc"),
    ]);
    let destination = tempfile::tempdir().unwrap();
    unpack(&bytes, destination.path(), 3).unwrap();
    assert_eq!(
        fs::read(destination.path().join("dir/item")).unwrap(),
        b"abc"
    );
    let rejected = tempfile::tempdir().unwrap();
    assert!(matches!(
        unpack(&bytes, rejected.path(), 2),
        Err(PluginBundleUnpackError::ExtractedBundleTooLarge {
            bytes: 3,
            max_bytes: 2
        })
    ));
    let repeated = archive(&[
        ("item", tar::EntryType::Regular, b"abc"),
        ("item", tar::EntryType::Regular, b"def"),
    ]);
    assert!(matches!(
        unpack(&repeated, rejected.path(), 5),
        Err(PluginBundleUnpackError::ExtractedBundleTooLarge {
            bytes: 6,
            max_bytes: 5
        })
    ));
    assert_eq!(fs::read(rejected.path().join("item")).unwrap(), b"abc");
}

#[test]
fn extracted_size_overflow_reports_saturation_without_mutating_the_counter() {
    let mut total = u64::MAX - 2;
    assert!(matches!(
        enforce_size(3, &mut total, u64::MAX),
        Err(PluginBundleUnpackError::ExtractedBundleTooLarge {
            bytes: u64::MAX,
            max_bytes: u64::MAX
        })
    ));
    assert_eq!(total, u64::MAX - 2);
}

#[test]
fn invalid_later_member_leaves_prior_files_in_the_owned_destination() {
    let destination = tempfile::tempdir().unwrap();
    let bytes = archive(&[
        ("./dir/item", tar::EntryType::Regular, b"kept"),
        ("../escape", tar::EntryType::Regular, b"bad"),
    ]);
    assert!(unpack(&bytes, destination.path(), 100).is_err());
    assert_eq!(
        fs::read(destination.path().join("dir/item")).unwrap(),
        b"kept"
    );
}

#[test]
fn malformed_gzip_and_unwritable_destination_preserve_contextual_errors() {
    let destination = tempfile::tempdir().unwrap();
    assert!(matches!(
        unpack(b"not gzip", destination.path(), 100),
        Err(PluginBundleUnpackError::Io { .. })
    ));
    let file = destination.path().join("file");
    fs::write(&file, b"occupied").unwrap();
    let err = unpack(&archive(&[]), &file, 100).unwrap_err();
    assert!(
        err.to_string()
            .starts_with("failed to create plugin bundle extraction directory:")
    );
    assert_eq!(fs::read(file).unwrap(), b"occupied");
}

#[test]
fn packing_checks_manifest_and_compressed_limit_and_preserves_file_bytes() {
    let source = tempfile::tempdir().unwrap();
    assert!(matches!(
        pack(source.path(), 1000),
        Err(PluginBundlePackError::InvalidPluginPath { .. })
    ));
    fs::create_dir(source.path().join(".codex-plugin")).unwrap();
    // The legacy manifest-file check is deliberately an existence check, as upstream.
    fs::write(source.path().join(".codex-plugin/plugin.json"), b"{").unwrap();
    fs::write(source.path().join("data"), b"contents").unwrap();
    assert!(matches!(
        pack(source.path(), 1),
        Err(PluginBundlePackError::ArchiveTooLarge { max_bytes: 1, .. })
    ));
    let bytes = pack(source.path(), 10000).unwrap();
    let destination = tempfile::tempdir().unwrap();
    unpack(&bytes, destination.path(), 10000).unwrap();
    assert_eq!(
        fs::read(destination.path().join("data")).unwrap(),
        b"contents"
    );
}

#[cfg(unix)]
#[test]
fn packing_rejects_filesystem_symlinks() {
    let source = tempfile::tempdir().unwrap();
    fs::create_dir(source.path().join(".codex-plugin")).unwrap();
    fs::write(source.path().join(".codex-plugin/plugin.json"), b"{}").unwrap();
    std::os::unix::fs::symlink(".codex-plugin/plugin.json", source.path().join("link")).unwrap();
    let err = pack(source.path(), 10000).unwrap_err();
    assert!(matches!(err, PluginBundlePackError::Io { .. }));
    assert!(
        err.to_string()
            .contains("unsupported plugin archive entry type")
    );
}
