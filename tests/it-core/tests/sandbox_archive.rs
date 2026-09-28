//! `ra_core::sandbox::archive`: what an archive is, and what unpacking one may cost.
//!
//! The ceilings are the interesting part. Their default is not "the built-in values" but "no
//! ceilings at all", because that is what a caller who says nothing gets from the reference; the
//! built-in values are what `SandboxArchiveLimits` itself is for. Getting that backwards would
//! quietly impose a limit on every caller who never asked for one.

use ra_core::sandbox::{
    CompressionScheme, DEFAULT_MAX_ARCHIVE_EXTRACTED_BYTES, DEFAULT_MAX_ARCHIVE_INPUT_BYTES,
    DEFAULT_MAX_ARCHIVE_MEMBERS, SandboxArchiveLimits,
};

#[test]
fn the_built_in_ceilings_are_the_ones_a_caller_opts_into() {
    let limits = SandboxArchiveLimits::default();

    assert_eq!(
        limits.max_input_bytes(),
        Some(DEFAULT_MAX_ARCHIVE_INPUT_BYTES)
    );
    assert_eq!(
        limits.max_extracted_bytes(),
        Some(DEFAULT_MAX_ARCHIVE_EXTRACTED_BYTES)
    );
    assert_eq!(limits.max_members(), Some(DEFAULT_MAX_ARCHIVE_MEMBERS));
    // The values themselves, so a change to any of them is a decision rather than a drift.
    assert_eq!(DEFAULT_MAX_ARCHIVE_INPUT_BYTES, 1024 * 1024 * 1024);
    assert_eq!(DEFAULT_MAX_ARCHIVE_EXTRACTED_BYTES, 4 * 1024 * 1024 * 1024);
    assert_eq!(DEFAULT_MAX_ARCHIVE_MEMBERS, 100_000);
}

#[test]
fn a_ceiling_can_be_turned_off_but_not_set_to_zero() {
    // Zero refuses every archive, including an empty one, and reads as "unlimited" at a glance.
    assert!(
        SandboxArchiveLimits::new()
            .with_max_input_bytes(Some(0))
            .is_err()
    );
    assert!(
        SandboxArchiveLimits::new()
            .with_max_extracted_bytes(Some(0))
            .is_err()
    );
    assert!(
        SandboxArchiveLimits::new()
            .with_max_members(Some(0))
            .is_err()
    );

    let relaxed = SandboxArchiveLimits::new()
        .with_max_input_bytes(None)
        .expect("limit")
        .with_max_extracted_bytes(Some(64))
        .expect("limit")
        .with_max_members(None)
        .expect("limit");
    assert_eq!(relaxed.max_input_bytes(), None);
    assert_eq!(relaxed.max_extracted_bytes(), Some(64));
    assert_eq!(relaxed.max_members(), None);
}

#[test]
fn a_format_is_read_from_the_last_extension_and_nothing_cleverer() {
    assert_eq!(
        CompressionScheme::from_file_name("bundle.tar"),
        Some(CompressionScheme::Tar)
    );
    assert_eq!(
        CompressionScheme::from_file_name("bundle.zip"),
        Some(CompressionScheme::Zip)
    );
    // `bundle.tar.gz` names `gz`, which is not one of the two. The reference reads the last
    // extension too, so a caller who means a compressed tar has to say so.
    assert_eq!(CompressionScheme::from_file_name("bundle.tar.gz"), None);
    assert_eq!(CompressionScheme::from_file_name("bundle"), None);
    assert_eq!(CompressionScheme::from_file_name(".tar"), None);
}

#[test]
fn a_format_written_down_reads_back() {
    for scheme in [CompressionScheme::Tar, CompressionScheme::Zip] {
        assert_eq!(CompressionScheme::parse(scheme.as_str()), Some(scheme));
        assert_eq!(
            serde_json::to_value(scheme).expect("render"),
            serde_json::Value::String(scheme.as_str().to_owned())
        );
    }
    assert_eq!(CompressionScheme::parse("gz"), None);
}
