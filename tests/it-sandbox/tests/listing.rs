//! `ra-sandbox::listing`: reading a directory listing back out of `ls -la`.
//!
//! A session that has to list as another account, or across a machine boundary, gets its answer as
//! text from whatever `ls` the sandbox image carries. The two implementations disagree about
//! devices and neither of them escapes a filename, so the parser has to be written against both and
//! has to skip a line that is not an entry rather than failing the whole listing over it. An entry
//! whose mode does not read is the exception: the reference fails the listing on it.
//!
//! The second half ports the reference's `tests/sandbox/test_parse_utils.py`, one test per case.

use ra_core::sandbox::{EntryKind, FileEntry, FileMode, PermissionsParseError};
use ra_sandbox::listing::try_parse_ls_la;

fn parse_ls_la_ok(output: &str, base: &str) -> Vec<FileEntry> {
    try_parse_ls_la(output, base).expect("the listing reads")
}

#[test]
fn a_coreutils_listing_is_read_into_entries() {
    let output = concat!(
        "total 12\n",
        "drwxr-xr-x  2 root root     4096 Jan  1 00:00 .\n",
        "drwxr-xr-x 20 root root     4096 Jan  1 00:00 ..\n",
        "drwxr-xr-x  2 1000 1000     4096 Jan  1 00:00 src\n",
        "-rw-r--r--  1 1000 1000      123 Jan  1 00:00 notes.md\n",
    );
    let entries = parse_ls_la_ok(output, "/workspace");

    // `.` and `..` are not contents, and the `total` header is not an entry.
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].path, "/workspace/src");
    assert_eq!(entries[0].kind, EntryKind::Directory);
    assert!(entries[0].permissions.directory);
    assert_eq!(entries[0].owner, "1000");
    assert_eq!(entries[1].path, "/workspace/notes.md");
    assert_eq!(entries[1].kind, EntryKind::File);
    assert_eq!(entries[1].size, 123);
}

#[test]
fn a_symlink_keeps_its_own_name_and_not_its_target() {
    let output = "lrwxrwxrwx  1 root root       12 Jan  1 00:00 entry -> src/main.rs\n";
    let entries = parse_ls_la_ok(output, "/workspace");

    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].path, "/workspace/entry");
    assert_eq!(entries[0].kind, EntryKind::Symlink);
    // The type marker is normalized away: only the rwx bits and directory-ness are modelled, and a
    // listing that failed to parse a link would lose the entry entirely.
    assert!(!entries[0].permissions.directory);
}

#[test]
fn a_name_with_spaces_survives_as_one_name() {
    let output = "-rw-r--r--  1 root root        7 Jan  1 00:00 my notes.md\n";
    let entries = parse_ls_la_ok(output, "/workspace");

    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].path, "/workspace/my notes.md");
}

#[test]
fn both_spellings_of_a_device_are_read_and_called_zero_bytes() {
    // GNU prints `major, minor` where a file prints its size, which shifts every later column;
    // BSD prints one hexadecimal word and shifts nothing. `stat` calls a device zero bytes.
    let gnu = "crw-rw-rw-  1 root root     1,   3 Jan  1 00:00 null\n";
    let bsd = "crw-rw-rw-  1 root root 0x3000002 Jan  1 00:00 null\n";

    for output in [gnu, bsd] {
        let entries = parse_ls_la_ok(output, "/dev");
        assert_eq!(entries.len(), 1, "{output}");
        assert_eq!(entries[0].path, "/dev/null");
        assert_eq!(entries[0].kind, EntryKind::Other);
        assert_eq!(entries[0].size, 0);
    }
}

#[test]
fn a_line_that_is_not_an_entry_is_skipped_rather_than_failing_the_listing() {
    let output = concat!(
        "ls: cannot access 'gone': No such file or directory\n",
        "\n",
        "-rw-r--r--  1 root root        7 Jan  1 00:00 kept.md\n",
    );
    let entries = parse_ls_la_ok(output, "/workspace");

    // This is output from a command, not a wire format. A parser that failed on an unfamiliar line
    // would lose a whole listing over one diagnostic.
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].path, "/workspace/kept.md");
}

#[test]
fn a_listing_of_the_filesystem_root_does_not_double_the_separator() {
    let output = "-rw-r--r--  1 root root        7 Jan  1 00:00 hostname\n";
    let entries = parse_ls_la_ok(output, "/");

    assert_eq!(entries[0].path, "/hostname");
}

// --- the reference's `test_parse_utils.py` --------------------------------------------------------

fn paths(entries: &[FileEntry]) -> Vec<&str> {
    entries.iter().map(|entry| entry.path.as_str()).collect()
}

fn exec(bits: u32) -> bool {
    bits & FileMode::Exec.bits() != 0
}

/// `test_parse_ls_la_preserves_absolute_file_paths`: listing a file prints its full path, which is
/// kept rather than re-attached to the base.
#[test]
fn a_file_listed_by_its_absolute_path_keeps_that_path() {
    let output = "-rwxr-xr-x 1 root root 48915747 Jan 1 00:00 /workspace/bin/tool\n";
    let entries = parse_ls_la_ok(output, "/workspace/bin/tool");

    assert_eq!(paths(&entries), ["/workspace/bin/tool"]);
    assert_eq!(entries[0].kind, EntryKind::File);
}

/// `test_parse_ls_la_prefixes_directory_entries_with_base`.
#[test]
fn a_directorys_entries_are_prefixed_with_the_base() {
    let output = concat!(
        "drwxr-xr-x 2 root root     4096 Jan  1 00:00 .\n",
        "drwxr-xr-x 3 root root     4096 Jan  1 00:00 ..\n",
        "-rw-r--r-- 1 root root      123 Jan  1 00:00 notes.md\n",
    );
    let entries = parse_ls_la_ok(output, "/workspace/docs");

    assert_eq!(paths(&entries), ["/workspace/docs/notes.md"]);
    assert_eq!(entries[0].kind, EntryKind::File);
}

/// `test_parse_ls_la_keeps_arrow_in_regular_file_names`: only a symlink's name is cut at ` -> `.
#[test]
fn an_arrow_in_a_regular_files_name_is_part_of_the_name() {
    let output = "-rw-r--r-- 1 root root 123 Jan 1 00:00 notes -> final.txt\n";
    let entries = parse_ls_la_ok(output, "/workspace/docs");

    assert_eq!(paths(&entries), ["/workspace/docs/notes -> final.txt"]);
    assert_eq!(entries[0].kind, EntryKind::File);
}

/// `test_parse_ls_la_accepts_special_permission_bits`: setuid, setgid and sticky, with and without
/// the execute bit beneath them.
#[test]
fn special_permission_bits_are_read_as_execute_or_not() {
    let output = concat!(
        "drwxrwxrwt 2 root root 4096 Jan 1 00:00 tmp\n",
        "-rwsr-sr-t 1 root root 123 Jan 1 00:00 setuid-tool\n",
        "-rwSr-Sr-T 1 root root 456 Jan 1 00:00 special-no-exec\n",
    );
    let entries = parse_ls_la_ok(output, "/");

    assert_eq!(
        paths(&entries),
        ["/tmp", "/setuid-tool", "/special-no-exec"]
    );
    assert!(entries[0].permissions.directory);
    assert!(exec(entries[0].permissions.other));
    assert!(exec(entries[1].permissions.owner));
    assert!(exec(entries[1].permissions.group));
    assert!(exec(entries[1].permissions.other));
    assert!(!exec(entries[2].permissions.owner));
    assert!(!exec(entries[2].permissions.group));
    assert!(!exec(entries[2].permissions.other));
}

/// `test_parse_ls_la_strips_trailing_alternate_access_markers`: `.` for an SELinux context, `+` for
/// an ACL, `@` for macOS extended attributes.
#[test]
fn a_trailing_alternate_access_marker_is_not_part_of_the_mode() {
    let output = concat!(
        "drwxr-xr-x. 2 root root 4096 Jan 1 00:00 selinux-dir\n",
        "-rw-r--r--+ 1 root root  123 Jan 1 00:00 acl-file\n",
        "-rw-r--r--@ 1 root root  456 Jan 1 00:00 xattr-file\n",
    );
    let entries = parse_ls_la_ok(output, "/");

    assert_eq!(
        paths(&entries),
        ["/selinux-dir", "/acl-file", "/xattr-file"]
    );
    assert!(entries[0].permissions.directory);
    assert!(entries[0].permissions.owner & FileMode::Read.bits() != 0);
    assert!(entries[1].permissions.owner & FileMode::Write.bits() != 0);
    assert!(entries[2].permissions.owner & FileMode::Read.bits() != 0);
}

/// `test_parse_ls_la_includes_gnu_device_nodes`.
#[test]
fn gnu_device_rows_shift_their_columns_and_keep_their_owners() {
    let output = concat!(
        "-rw-r--r-- 1 root root      123 Jan  1 00:00 regular.txt\n",
        "crw-rw-rw- 1 root root     1, 3 Jan  1 00:00 null\n",
        "brw-rw---- 1 root disk     8, 0 Jan  1 00:00 sda\n",
    );
    let entries = parse_ls_la_ok(output, "/dev");

    assert_eq!(
        paths(&entries),
        ["/dev/regular.txt", "/dev/null", "/dev/sda"]
    );
    assert_eq!(entries[1].kind, EntryKind::Other);
    assert_eq!(entries[1].size, 0);
    assert_eq!(entries[2].owner, "root");
    assert_eq!(entries[2].group, "disk");
}

/// `test_parse_ls_la_includes_bsd_device_nodes`.
#[test]
fn bsd_device_rows_keep_their_columns_and_their_owners() {
    let output = concat!(
        "-rw-r--r--  1 root  wheel           123 Jan  1 00:00 regular.txt\n",
        "crw-rw-rw-  1 root  wheel     0x3000002 Jan  1 00:00 null\n",
        "brw-r-----  1 root  operator  0x1000000 Jan  1 00:00 disk0\n",
    );
    let entries = parse_ls_la_ok(output, "/dev");

    assert_eq!(
        paths(&entries),
        ["/dev/regular.txt", "/dev/null", "/dev/disk0"]
    );
    assert_eq!(entries[1].kind, EntryKind::Other);
    assert_eq!(entries[1].size, 0);
    assert_eq!(entries[2].owner, "root");
    assert_eq!(entries[2].group, "operator");
}

/// `test_parse_ls_la_rejects_special_permission_bits_in_wrong_position`: a special-bit letter in a
/// triplet it does not belong to fails the listing rather than dropping the entry.
#[test]
fn a_special_bit_in_the_wrong_triplet_fails_the_listing() {
    for mode in [
        "-rwTr--r--",
        "-rwxrwTr--",
        "-rwxrwxr-S",
        "-rwtr--r--",
        "-rwxrwtr--",
        "-rwxrwxr-s",
    ] {
        let output = format!("{mode} 1 root root 123 Jan 1 00:00 invalid\n");
        let error = try_parse_ls_la(&output, "/").expect_err(mode);
        assert!(
            matches!(error, PermissionsParseError::ExecFlag { .. }),
            "{mode}: {error:?}"
        );
        assert!(
            error.to_string().starts_with("invalid exec flag"),
            "{error}"
        );
    }
}

/// The function callers used before `try_parse_ls_la` keeps its signature and its behaviour: a row
/// whose mode does not read is left out, and the rest of the listing stands.
#[test]
#[allow(deprecated)]
fn the_lenient_parser_keeps_leaving_out_a_row_it_cannot_read() {
    let output = concat!(
        "-rw-r--r-- 1 root root 1 Jan 1 00:00 fine.txt\n",
        "-rwTr--r-- 1 root root 1 Jan 1 00:00 odd\n",
    );

    let entries: Vec<FileEntry> = ra_sandbox::listing::parse_ls_la(output, "/workspace");

    assert_eq!(paths(&entries), ["/workspace/fine.txt"]);
}
