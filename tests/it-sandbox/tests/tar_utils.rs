//! `ra-sandbox::tar_utils`: the shared tar policy, the extractor a restore runs, prefix stripping,
//! and the shell `tar` exclusions.
//!
//! The reference's `test_tar_utils.py` and `test_tar_workspace.py`, case by case. Archives are built
//! with raw header names, so a name the `tar` crate would normalize or refuse on the way in — `.`,
//! `./workspace`, `C:\tmp\evil.txt` — reaches the policy exactly as written.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use ra_sandbox::tar_utils::{
    SafeExtractError, StripPrefixError, TarValidation, UnsafeTarMember, safe_extract_tarfile,
    shell_tar_exclude_args, should_skip_tar_member, strip_tar_member_prefix, validate_tar_bytes,
};

/// One member to write.
struct Member {
    name: &'static str,
    kind: tar::EntryType,
    payload: Vec<u8>,
    link: &'static str,
    mode: u32,
}

fn dir(name: &'static str) -> Member {
    Member {
        name,
        kind: tar::EntryType::Directory,
        payload: Vec::new(),
        link: "",
        mode: 0o755,
    }
}

fn file(name: &'static str, payload: &[u8]) -> Member {
    Member {
        name,
        kind: tar::EntryType::Regular,
        payload: payload.to_vec(),
        link: "",
        mode: 0o644,
    }
}

fn file_with_mode(name: &'static str, payload: &[u8], mode: u32) -> Member {
    Member {
        mode,
        ..file(name, payload)
    }
}

fn symlink(name: &'static str, target: &'static str) -> Member {
    Member {
        name,
        kind: tar::EntryType::Symlink,
        payload: Vec::new(),
        link: target,
        mode: 0o777,
    }
}

fn hardlink(name: &'static str, target: &'static str) -> Member {
    Member {
        kind: tar::EntryType::Link,
        ..symlink(name, target)
    }
}

fn fifo(name: &'static str) -> Member {
    Member {
        kind: tar::EntryType::Fifo,
        ..dir(name)
    }
}

/// An uncompressed archive of `members`, names written into the header byte for byte.
fn tar_bytes(members: Vec<Member>) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for member in members {
        let mut header = tar::Header::new_ustar();
        header.set_entry_type(member.kind);
        header.set_mode(member.mode);
        header.set_size(member.payload.len() as u64);
        let old = header.as_old_mut();
        old.name[..member.name.len()].copy_from_slice(member.name.as_bytes());
        old.linkname[..member.link.len()].copy_from_slice(member.link.as_bytes());
        header.set_cksum();
        builder
            .append(&header, member.payload.as_slice())
            .expect("member");
    }
    builder.into_inner().expect("archive")
}

fn validate(raw: &[u8]) -> Result<(), UnsafeTarMember> {
    validate_tar_bytes(raw, &TarValidation::new())
}

fn strict() -> TarValidation {
    TarValidation::new().with_external_symlink_targets(false)
}

/// The reason a validation refused `raw` for.
fn refusal(result: Result<(), UnsafeTarMember>) -> String {
    result.expect_err("refused").reason().to_owned()
}

/// The reason an extraction refused for.
fn extract_refusal(result: Result<(), SafeExtractError>) -> String {
    match result.expect_err("refused") {
        SafeExtractError::Unsafe(member) => member.reason().to_owned(),
        other => panic!("expected a refused member, got {other}"),
    }
}

fn extract(raw: &[u8], root: &Path) {
    safe_extract_tarfile(raw, root, true).expect("extract");
}

fn mode_of(path: &Path) -> u32 {
    std::fs::metadata(path)
        .expect("metadata")
        .permissions()
        .mode()
        & 0o7777
}

// --- validate_tar_bytes ------------------------------------------------------------------------

/// `test_safe_extract_tarfile_preserves_venv_style_symlinks`
#[test]
fn a_virtual_environments_links_are_restored_as_links() {
    let raw = tar_bytes(vec![
        dir("."),
        dir("./uv-project"),
        dir("./uv-project/.venv"),
        dir("./uv-project/.venv/bin"),
        dir("./uv-project/.venv/lib"),
        file("./uv-project/main.py", b"print(\"snapshot smoke\")\n"),
        symlink("./uv-project/.venv/lib64", "lib"),
        symlink("./uv-project/.venv/bin/python3", "/usr/local/bin/python3"),
        symlink("./uv-project/.venv/bin/python", "python3"),
    ]);
    let root = tempfile::tempdir().expect("temp");

    validate(&raw).expect("valid");
    extract(&raw, root.path());

    let project = root.path().join("uv-project");
    assert_eq!(
        std::fs::read(project.join("main.py")).expect("read"),
        b"print(\"snapshot smoke\")\n"
    );
    let venv = project.join(".venv");
    assert_eq!(
        std::fs::read_link(venv.join("lib64")).expect("link"),
        Path::new("lib")
    );
    assert_eq!(
        std::fs::read_link(venv.join("bin/python3")).expect("link"),
        Path::new("/usr/local/bin/python3")
    );
    assert_eq!(
        std::fs::read_link(venv.join("bin/python")).expect("link"),
        Path::new("python3")
    );
}

/// `test_validate_tar_bytes_rejects_root_symlink`
#[test]
fn the_archive_root_cannot_be_a_link() {
    assert_eq!(
        refusal(validate(&tar_bytes(vec![symlink(".", "/tmp/outside")]))),
        "archive root symlink"
    );
}

/// `test_validate_tar_bytes_rejects_windows_drive_member_paths`, both parameters, and the UNC
/// spellings Python 3.11's `PureWindowsPath` also reads as a drive.
#[test]
fn a_member_on_a_windows_drive_is_refused() {
    for name in [
        "C:/tmp/evil.txt",
        r"C:\tmp\evil.txt",
        "//server/share/evil.txt",
        "//server/",
        "//?/evil.txt",
        "//./device",
    ] {
        let raw = tar_bytes(vec![file(name, b"evil")]);
        assert_eq!(refusal(validate(&raw)), "windows drive path", "{name}");
    }
    // Not drives, checked against the same interpreter: a server with no separator after it, a
    // third separator, and an empty share. They are refused all the same, as absolute paths.
    for name in ["//server", "///x", "//a//b"] {
        let raw = tar_bytes(vec![file(name, b"evil")]);
        assert_eq!(refusal(validate(&raw)), "absolute path", "{name}");
    }
}

/// `test_validate_tar_bytes_rejects_windows_separator_member_paths`, all three parameters.
#[test]
fn a_member_named_with_a_backslash_is_refused() {
    for name in [r"..\evil.txt", r"\evil.txt", r"nested\evil.txt"] {
        let raw = tar_bytes(vec![file(name, b"evil")]);
        assert_eq!(refusal(validate(&raw)), "windows path separator", "{name}");
    }
}

/// `test_validate_tar_bytes_rejects_member_under_non_directory_member`
#[test]
fn a_member_beneath_a_file_is_refused() {
    let raw = tar_bytes(vec![
        file("nested/hello.txt", b"hello"),
        file("nested", b"not a directory"),
    ]);

    assert_eq!(
        refusal(validate(&raw)),
        "archive path descends through non-directory: nested"
    );
}

/// `test_validate_tar_bytes_rejects_absolute_symlink_target_in_strict_mode`,
/// `…_rejects_parent_escape_symlink_target_in_strict_mode` and
/// `…_allows_internal_symlink_target_in_strict_mode`.
#[test]
fn strict_mode_keeps_link_targets_inside_the_archive() {
    let absolute = tar_bytes(vec![symlink("leak", "/etc/passwd")]);
    assert_eq!(
        refusal(validate_tar_bytes(&absolute, &strict())),
        "absolute symlink target not allowed: /etc/passwd"
    );
    let climbing = tar_bytes(vec![
        dir("nested"),
        symlink("nested/leak", "../../etc/passwd"),
    ]);
    assert_eq!(
        refusal(validate_tar_bytes(&climbing, &strict())),
        "symlink target escapes archive root: ../../etc/passwd"
    );
    let internal = tar_bytes(vec![
        dir("nested"),
        symlink("nested/python", "../bin/python3"),
    ]);
    validate_tar_bytes(&internal, &strict()).expect("stays inside");

    // And by default a target is only metadata: both of the refused ones are accepted.
    validate(&absolute).expect("external targets allowed");
    validate(&climbing).expect("external targets allowed");
}

/// `test_validate_tar_bytes_rejects_members_under_archive_symlink`
#[test]
fn a_member_beneath_a_link_from_the_same_archive_is_refused() {
    let raw = tar_bytes(vec![
        symlink("escape", "/tmp/outside"),
        file("escape/pwned.txt", b"pwned"),
    ]);

    assert_eq!(
        refusal(validate(&raw)),
        "archive path descends through symlink: escape"
    );
}

/// `test_validate_tar_bytes_can_reject_specific_symlink_path`,
/// `…_specific_symlink_rejection_normalizes_dot_prefix` and
/// `…_specific_symlink_rejection_does_not_reject_children`.
#[test]
fn a_link_can_be_refused_at_one_path_and_only_there() {
    let workspace = TarValidation::new().rejecting_symlinks_at(["workspace"]);

    let raw = tar_bytes(vec![symlink("workspace", "/tmp/outside")]);
    assert_eq!(
        refusal(validate_tar_bytes(&raw, &workspace)),
        "symlink member not allowed: workspace"
    );
    let dotted = tar_bytes(vec![symlink("./workspace", "/tmp/outside")]);
    assert_eq!(
        refusal(validate_tar_bytes(&dotted, &workspace)),
        "symlink member not allowed: workspace"
    );
    let child = tar_bytes(vec![
        dir("workspace"),
        symlink("workspace/link", "/tmp/outside"),
    ]);
    validate_tar_bytes(&child, &workspace).expect("a link under the path is not the path");
}

/// `test_validate_tar_bytes_rejects_members_overlapping_protected_path`, all three parameters,
/// `…_rejects_non_directory_ancestor_of_protected_path` and
/// `…_allows_directory_ancestor_of_protected_path`.
#[test]
fn a_protected_path_can_be_neither_written_into_nor_replaced() {
    let remote = TarValidation::new().rejecting(["remote"]);
    for member in [
        file("remote/data.txt", b"payload"),
        symlink("remote/link", "../outside"),
        file("remote", b"payload"),
    ] {
        let name = member.name;
        assert_eq!(
            refusal(validate_tar_bytes(&tar_bytes(vec![member]), &remote)),
            "archive member overlaps protected path: remote",
            "{name}"
        );
    }

    let nested = TarValidation::new().rejecting(["remote/nested"]);
    assert_eq!(
        refusal(validate_tar_bytes(
            &tar_bytes(vec![file("remote", b"payload")]),
            &nested
        )),
        "archive member overlaps protected path: remote/nested"
    );
    validate_tar_bytes(&tar_bytes(vec![dir("remote")]), &nested)
        .expect("a directory on the way to a protected path is not in its way");
}

/// `test_validate_tar_bytes_rejects_unsupported_tar_member_types`, both parameters.
#[test]
fn hard_links_and_special_files_are_refused() {
    assert_eq!(
        refusal(validate(&tar_bytes(vec![hardlink(
            "hardlink",
            "target.txt"
        )]))),
        "hardlink member not allowed"
    );
    assert_eq!(
        refusal(validate(&tar_bytes(vec![fifo("pipe")]))),
        "unsupported member type"
    );
}

/// `test_validate_tar_bytes_ignores_skipped_unsafe_member`
#[test]
fn a_skipped_member_is_not_checked_at_all() {
    let raw = tar_bytes(vec![symlink(".runtime/escape", "/tmp/outside")]);

    validate_tar_bytes(&raw, &strict().skipping([".runtime"]))
        .expect("skipped before any rule applies");
    assert!(validate_tar_bytes(&raw, &strict()).is_err());
}

#[test]
fn bytes_that_are_not_an_archive_are_named_as_such() {
    let error = validate(&[0x1f, 0x8b, 0, 1, 2]).expect_err("refused");
    assert_eq!(error.member(), "<tar>");
    assert_eq!(error.reason(), "invalid tar stream");
}

#[test]
fn a_skip_path_matches_either_spelling_of_a_member_name() {
    assert!(should_skip_tar_member("./cache/file", ["cache"], None));
    assert!(should_skip_tar_member("cache", ["./cache/"], None));
    assert!(!should_skip_tar_member("cached/file", ["cache"], None));
    // The workspace directory's own name as the first component is matched too, when it is known.
    assert!(should_skip_tar_member(
        "workspace/cache/file",
        ["cache"],
        Some("workspace")
    ));
    assert!(!should_skip_tar_member(
        "workspace/cache/file",
        ["cache"],
        None
    ));
    // An empty skip path is everything, as it is in the reference.
    assert!(should_skip_tar_member("anything", [""], None));
}

// --- strip_tar_member_prefix -------------------------------------------------------------------

/// Every member name in `raw`, as the header or its `path` record spells it.
fn raw_names(raw: &[u8]) -> Vec<String> {
    let mut archive = tar::Archive::new(raw);
    archive
        .entries()
        .expect("entries")
        .map(|entry| {
            String::from_utf8(entry.expect("entry").path_bytes().into_owned()).expect("utf-8")
        })
        .collect()
}

/// `test_strip_tar_member_prefix_returns_workspace_relative_archive`
#[test]
fn members_under_the_prefix_are_named_from_the_workspace_root() {
    let raw = tar_bytes(vec![
        dir("workspace"),
        dir("workspace/pkg"),
        file("workspace/pkg/main.py", b"print('hello')\n"),
        symlink("workspace/pkg/python", "python3"),
    ]);

    let normalized = strip_tar_member_prefix(&raw, "workspace").expect("strip");

    // Directories carry the trailing separator Python writes and strips again when it reads, so
    // the reference's `getnames()` sees `.`, `pkg`, `pkg/main.py`, `pkg/python`.
    assert_eq!(
        raw_names(&normalized),
        ["./", "pkg/", "pkg/main.py", "pkg/python"]
    );
    let mut archive = tar::Archive::new(normalized.as_slice());
    let link = archive
        .entries()
        .expect("entries")
        .map(|entry| entry.expect("entry"))
        .find(|entry| entry.header().entry_type().is_symlink())
        .expect("the link survived");
    assert_eq!(link.link_name_bytes().as_deref(), Some(&b"python3"[..]));
}

/// `test_strip_tar_member_prefix_rewrites_pax_path_headers`
#[test]
fn a_long_name_is_carried_in_a_path_record_after_stripping() {
    let long = format!("workspace/{}.txt", "a".repeat(120));
    let stripped = format!("{}.txt", "a".repeat(120));
    let mut builder = tar::Builder::new(Vec::new());
    builder
        .append_pax_extensions([("path", long.as_bytes())])
        .expect("path record");
    let mut header = tar::Header::new_ustar();
    header.set_size(7);
    header.set_mode(0o644);
    header.as_old_mut().name[..100].copy_from_slice(&long.as_bytes()[..100]);
    header.set_cksum();
    builder.append(&header, &b"payload"[..]).expect("member");
    let raw = builder.into_inner().expect("archive");

    let normalized = strip_tar_member_prefix(&raw, "workspace").expect("strip");

    let mut archive = tar::Archive::new(normalized.as_slice());
    let mut entries = archive.entries().expect("entries");
    let mut entry = entries.next().expect("one member").expect("entry");
    assert_eq!(entry.path_bytes().as_ref(), stripped.as_bytes());
    let records: Vec<(String, Vec<u8>)> = entry
        .pax_extensions()
        .expect("records")
        .expect("a path record was written")
        .map(|record| {
            let record = record.expect("record");
            (
                record.key().expect("key").to_owned(),
                record.value_bytes().to_vec(),
            )
        })
        .collect();
    assert_eq!(
        records,
        [("path".to_owned(), stripped.clone().into_bytes())]
    );
    let mut payload = Vec::new();
    std::io::Read::read_to_end(&mut entry, &mut payload).expect("payload");
    assert_eq!(payload, b"payload");
    assert!(entries.next().is_none());
}

#[test]
fn a_member_outside_the_prefix_is_refused_rather_than_dropped() {
    let raw = tar_bytes(vec![dir("workspace"), file("elsewhere/file.txt", b"x")]);

    let Err(StripPrefixError::Unsafe(member)) = strip_tar_member_prefix(&raw, "workspace") else {
        panic!("expected a refused member");
    };
    assert_eq!(member.member(), "elsewhere/file.txt");
    assert_eq!(
        member.reason(),
        "member does not start with prefix: workspace"
    );

    assert!(matches!(
        strip_tar_member_prefix(&raw, "./"),
        Err(StripPrefixError::EmptyPrefix)
    ));
}

// --- safe_extract_tarfile ----------------------------------------------------------------------

/// `test_safe_extract_tarfile_can_rehydrate_existing_leaf_symlink`
#[test]
fn a_link_already_extracted_is_replaced_by_the_next_one() {
    let root = tempfile::tempdir().expect("temp");

    extract(
        &tar_bytes(vec![symlink("link.txt", "/usr/local/bin/python3")]),
        root.path(),
    );
    assert_eq!(
        std::fs::read_link(root.path().join("link.txt")).expect("link"),
        Path::new("/usr/local/bin/python3")
    );
    extract(
        &tar_bytes(vec![symlink("link.txt", "target-v2.txt")]),
        root.path(),
    );
    assert_eq!(
        std::fs::read_link(root.path().join("link.txt")).expect("link"),
        Path::new("target-v2.txt")
    );
}

/// `test_safe_extract_tarfile_rejects_external_symlink_target_in_strict_mode`
#[test]
fn a_strict_extraction_refuses_a_link_out_of_the_archive() {
    let root = tempfile::tempdir().expect("temp");

    let reason = extract_refusal(safe_extract_tarfile(
        &tar_bytes(vec![symlink("link.txt", "/etc/passwd")]),
        root.path(),
        false,
    ));

    assert_eq!(reason, "absolute symlink target not allowed: /etc/passwd");
    assert!(!root.path().join("link.txt").is_symlink());
}

/// `test_safe_extract_tarfile_can_replace_existing_leaf_file_with_symlink`,
/// `…_existing_leaf_symlink_with_file`, `…_existing_leaf_symlink_with_directory` and
/// `…_existing_leaf_file_with_directory`.
#[test]
fn a_leaf_can_change_between_file_link_and_directory() {
    let root = tempfile::tempdir().expect("temp");
    let root = root.path();

    extract(&tar_bytes(vec![file("link.txt", b"not a link")]), root);
    extract(&tar_bytes(vec![symlink("link.txt", "target.txt")]), root);
    assert_eq!(
        std::fs::read_link(root.join("link.txt")).expect("link"),
        Path::new("target.txt")
    );

    extract(
        &tar_bytes(vec![symlink("python", "/usr/local/bin/python3")]),
        root,
    );
    extract(&tar_bytes(vec![file("python", b"real file")]), root);
    assert!(!root.join("python").is_symlink());
    assert_eq!(
        std::fs::read(root.join("python")).expect("read"),
        b"real file"
    );

    extract(&tar_bytes(vec![symlink("bin", "/usr/local/bin")]), root);
    extract(
        &tar_bytes(vec![dir("bin"), file("bin/python", b"real file")]),
        root,
    );
    assert!(root.join("bin").is_dir() && !root.join("bin").is_symlink());
    assert_eq!(
        std::fs::read(root.join("bin/python")).expect("read"),
        b"real file"
    );

    extract(&tar_bytes(vec![file("sbin", b"not a directory")]), root);
    extract(
        &tar_bytes(vec![dir("sbin"), file("sbin/python", b"real file")]),
        root,
    );
    assert!(root.join("sbin").is_dir());
    assert_eq!(
        std::fs::read(root.join("sbin/python")).expect("read"),
        b"real file"
    );
}

/// `test_safe_extract_tarfile_rejects_existing_leaf_directory_for_symlink`
#[test]
fn a_real_directory_is_not_replaced_by_a_link() {
    let root = tempfile::tempdir().expect("temp");
    std::fs::create_dir(root.path().join("link.txt")).expect("mkdir");

    let reason = extract_refusal(safe_extract_tarfile(
        &tar_bytes(vec![symlink("link.txt", "target.txt")]),
        root.path(),
        true,
    ));

    assert_eq!(reason, "destination directory already exists: link.txt");
}

/// `test_safe_extract_tarfile_rejects_preexisting_symlink_parent` and
/// `…_rejects_symlink_under_preexisting_symlink_parent`.
#[test]
fn nothing_is_written_through_a_link_already_on_disk() {
    for member in [
        file("escape/pwned.txt", b"pwned"),
        symlink("escape/nested/link.txt", "target.txt"),
    ] {
        let scratch = tempfile::tempdir().expect("temp");
        let outside = scratch.path().join("outside");
        let root = scratch.path().join("root");
        std::fs::create_dir(&outside).expect("mkdir");
        std::fs::create_dir(&root).expect("mkdir");
        std::os::unix::fs::symlink(&outside, root.join("escape")).expect("link");

        let reason = extract_refusal(safe_extract_tarfile(&tar_bytes(vec![member]), &root, true));

        assert!(
            reason == "path escapes root after resolution" || reason == "symlink in parent path",
            "{reason}"
        );
        assert_eq!(
            std::fs::read_dir(&outside).expect("list").count(),
            0,
            "nothing reached the other side"
        );
    }
}

/// `test_safe_extract_tarfile_restores_regular_file_modes`, all eleven parameters.
#[test]
fn an_extracted_file_keeps_only_the_permissions_the_policy_allows() {
    for (archived, expected) in [
        (0o755, 0o755),
        (0o644, 0o644),
        (0o600, 0o600),
        (0o700, 0o700),
        (0o444, 0o644),
        (0o000, 0o600),
        (0o777, 0o755),
        (0o655, 0o644),
        (0o4755, 0o755),
        (0o2755, 0o755),
        (0o1755, 0o755),
    ] {
        let root = tempfile::tempdir().expect("temp");
        extract(
            &tar_bytes(vec![file_with_mode("run.sh", b"#!/bin/sh\n", archived)]),
            root.path(),
        );
        assert_eq!(
            mode_of(&root.path().join("run.sh")),
            expected,
            "{archived:o}"
        );
    }
}

/// `test_safe_extract_tarfile_keeps_workspace_scripts_executable` and
/// `…_restores_mode_when_replacing_an_existing_file`.
#[test]
fn a_script_stays_executable_and_a_replaced_file_takes_the_new_mode() {
    let root = tempfile::tempdir().expect("temp");
    extract(
        &tar_bytes(vec![
            dir("."),
            dir("./bin"),
            file_with_mode("./bin/start", b"#!/bin/sh\necho hi\n", 0o755),
            file_with_mode("./README.md", b"# readme\n", 0o644),
        ]),
        root.path(),
    );
    assert_eq!(mode_of(&root.path().join("bin/start")) & 0o100, 0o100);
    assert_eq!(mode_of(&root.path().join("README.md")) & 0o111, 0);

    extract(
        &tar_bytes(vec![file_with_mode("run.sh", b"v1\n", 0o644)]),
        root.path(),
    );
    assert_eq!(mode_of(&root.path().join("run.sh")), 0o644);
    extract(
        &tar_bytes(vec![file_with_mode("run.sh", b"v2\n", 0o755)]),
        root.path(),
    );
    assert_eq!(
        std::fs::read(root.path().join("run.sh")).expect("read"),
        b"v2\n"
    );
    assert_eq!(mode_of(&root.path().join("run.sh")), 0o755);
}

// --- shell_tar_exclude_args --------------------------------------------------------------------

/// `test_tar_workspace.py::test_shell_tar_exclude_args_skips_empty_and_dot_paths`
#[test]
fn empty_and_root_paths_exclude_nothing() {
    assert!(shell_tar_exclude_args(["", ".", "/"]).is_empty());
}

/// `test_tar_workspace.py::test_shell_tar_exclude_args_sorts_and_adds_plain_and_dot_prefixed_patterns`
#[test]
fn each_path_is_excluded_in_both_spellings_in_sorted_order() {
    assert_eq!(
        shell_tar_exclude_args(["logs/events.jsonl", "cache dir/file.txt"]),
        [
            "--exclude='cache dir/file.txt'",
            "--exclude='./cache dir/file.txt'",
            "--exclude=logs/events.jsonl",
            "--exclude=./logs/events.jsonl",
        ]
    );
}

/// `test_tar_workspace.py::test_shell_tar_exclude_args_normalizes_absolute_paths`
#[test]
fn an_absolute_path_is_excluded_relative_to_the_root() {
    assert_eq!(
        shell_tar_exclude_args(["/tmp/workspace/cache"]),
        [
            "--exclude=tmp/workspace/cache",
            "--exclude=./tmp/workspace/cache",
        ]
    );
}

#[test]
fn empty_streams_are_refused_but_empty_archives_are_valid() {
    use std::io::Write;
    for raw in [Vec::new(), tar_bytes(vec![])] {
        let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gzip.write_all(&raw).unwrap();
        let mut bzip = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::default());
        bzip.write_all(&raw).unwrap();
        let mut xz =
            lzma_rust2::XzWriter::new(Vec::new(), lzma_rust2::XzOptions::with_preset(6)).unwrap();
        xz.write_all(&raw).unwrap();
        for encoded in [
            raw.clone(),
            gzip.finish().unwrap(),
            bzip.finish().unwrap(),
            xz.finish().unwrap(),
        ] {
            if raw.is_empty() {
                let error = validate(&encoded).expect_err("empty stream");
                assert_eq!(error.member(), "<tar>");
                assert_eq!(error.reason(), "invalid tar stream");
                assert!(strip_tar_member_prefix(&encoded, "workspace").is_err());
            } else {
                validate(&encoded).expect("valid empty archive");
                strip_tar_member_prefix(&encoded, "workspace").expect("valid empty archive");
            }
        }
    }
}

#[test]
fn prefix_rewriting_preserves_signed_gnu_times_in_pax() {
    for time in [-1_i128, -123456789, 0, (1_i128 << 33) - 1, 1_i128 << 33] {
        let mut header = tar::Header::new_gnu();
        header.set_path("workspace/file").unwrap();
        header.set_size(1);
        header.set_mode(0o644);
        let bytes = time.to_be_bytes();
        header.as_old_mut().mtime.copy_from_slice(&bytes[4..]);
        header.as_old_mut().mtime[0] = if time < 0 { 0xff } else { 0x80 };
        header.set_cksum();
        let mut builder = tar::Builder::new(Vec::new());
        builder.append(&header, &b"x"[..]).unwrap();
        let output = strip_tar_member_prefix(&builder.into_inner().unwrap(), "workspace").unwrap();
        let mut archive = tar::Archive::new(output.as_slice());
        let mut entries = archive.entries().unwrap();
        let mut entry = entries.next().unwrap().unwrap();
        assert_eq!(entry.path_bytes().as_ref(), b"file");
        let pax_time = entry
            .pax_extensions()
            .unwrap()
            .into_iter()
            .flatten()
            .map(|record| record.unwrap())
            .find(|record| record.key().unwrap() == "mtime")
            .map(|record| record.value().unwrap().to_owned());
        if (0..(1_i128 << 33)).contains(&time) {
            assert_eq!(
                entry.header().mtime().unwrap(),
                u64::try_from(time).unwrap()
            );
            assert_eq!(pax_time, None);
        } else {
            assert_eq!(entry.header().mtime().unwrap(), 0);
            assert_eq!(pax_time, Some(time.to_string()));
        }
        let mut payload = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut payload).unwrap();
        assert_eq!(payload, b"x");
    }
}
