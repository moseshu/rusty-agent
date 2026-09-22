//! `ra-sandbox::listing`: reading a directory listing back out of `ls -la`.
//!
//! A session that has to list as another account, or across a machine boundary, gets its answer as
//! text from whatever `ls` the sandbox image carries. The two implementations disagree about
//! devices and neither of them escapes a filename, so the parser has to be written against both and
//! has to skip what it cannot read rather than failing the whole listing over one odd line.

use ra_core::sandbox::EntryKind;
use ra_sandbox::listing::parse_ls_la;

#[test]
fn a_coreutils_listing_is_read_into_entries() {
    let output = concat!(
        "total 12\n",
        "drwxr-xr-x  2 root root     4096 Jan  1 00:00 .\n",
        "drwxr-xr-x 20 root root     4096 Jan  1 00:00 ..\n",
        "drwxr-xr-x  2 1000 1000     4096 Jan  1 00:00 src\n",
        "-rw-r--r--  1 1000 1000      123 Jan  1 00:00 notes.md\n",
    );
    let entries = parse_ls_la(output, "/workspace");

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
    let entries = parse_ls_la(output, "/workspace");

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
    let entries = parse_ls_la(output, "/workspace");

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
        let entries = parse_ls_la(output, "/dev");
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
    let entries = parse_ls_la(output, "/workspace");

    // This is output from a command, not a wire format. A parser that failed on an unfamiliar line
    // would lose a whole listing over one diagnostic.
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].path, "/workspace/kept.md");
}

#[test]
fn a_listing_of_the_filesystem_root_does_not_double_the_separator() {
    let output = "-rw-r--r--  1 root root        7 Jan  1 00:00 hostname\n";
    let entries = parse_ls_la(output, "/");

    assert_eq!(entries[0].path, "/hostname");
}
