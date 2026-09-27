//! `ra-sandbox::archive`: unpacking an archive a caller handed in.
//!
//! An archive is input that names its own destinations, so almost everything here is refusal: a
//! member that climbs out with `..`, one that spells an absolute path or a Windows drive, a link in
//! either direction, the same path twice, a path that descends through a file, and a path whose
//! parent is already a symlink in the workspace. The session underneath is a recorder, so what was
//! written — and what was not — is visible without a real workspace.

use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;
use ra_core::sandbox::{
    AsUser, CompressionScheme, EntryKind, ErrorCode, ExecRequest, ExecResult, FileEntry, Manifest,
    Permissions, SandboxArchiveLimits, SandboxError, SandboxResult, SandboxSession,
    SandboxSessionState, SessionResources, Snapshot,
};
use ra_sandbox::archive::WorkspaceArchiveExtractor;
use rstest::rstest;

/// Where these archives are unpacked.
const ARCHIVE_PATH: &str = "/workspace/incoming/bundle.tar";

/// One thing the extractor asked the session to do.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Call {
    Mkdir(String),
    Write(String, Vec<u8>),
}

/// A session that records what it was asked to write, and lists what it was told to hold.
struct RecordingSession {
    resources: SessionResources,
    state: SandboxSessionState,
    calls: Mutex<Vec<Call>>,
    listings: BTreeMap<String, Vec<FileEntry>>,
    /// Every directory listed, in order.
    listed: Mutex<Vec<String>>,
}

impl RecordingSession {
    fn new() -> Self {
        Self {
            resources: SessionResources::new(),
            state: SandboxSessionState::new(
                "recording",
                Snapshot::noop(),
                Manifest::new().with_root("/workspace"),
            ),
            calls: Mutex::new(Vec::new()),
            listings: BTreeMap::new(),
            listed: Mutex::new(Vec::new()),
        }
    }

    /// Says that `directory` already holds `name`, as something of `kind`.
    fn holding(mut self, directory: &str, name: &str, kind: EntryKind) -> Self {
        self.listings.insert(
            directory.to_owned(),
            vec![
                FileEntry::new(format!("{directory}/{name}"), Permissions::default())
                    .with_kind(kind),
            ],
        );
        self
    }

    fn calls(&self) -> Vec<Call> {
        self.calls.lock().expect("calls").clone()
    }

    /// The paths written, in order.
    fn writes(&self) -> Vec<String> {
        self.calls()
            .into_iter()
            .filter_map(|call| match call {
                Call::Write(path, _) => Some(path),
                Call::Mkdir(_) => None,
            })
            .collect()
    }

    /// What was written to one path.
    fn written(&self, path: &str) -> Option<Vec<u8>> {
        self.calls().into_iter().find_map(|call| match call {
            Call::Write(written, bytes) if written == path => Some(bytes),
            _ => None,
        })
    }

    /// The directories created, in order.
    fn mkdirs(&self) -> Vec<String> {
        self.calls()
            .into_iter()
            .filter_map(|call| match call {
                Call::Mkdir(path) => Some(path),
                Call::Write(..) => None,
            })
            .collect()
    }
}

#[async_trait]
impl SandboxSession for RecordingSession {
    fn backend_id(&self) -> &str {
        "recording"
    }

    fn resources(&self) -> &SessionResources {
        &self.resources
    }

    fn state(&self) -> SandboxSessionState {
        self.state.clone()
    }

    async fn exec(&self, _request: ExecRequest) -> SandboxResult<ExecResult> {
        Ok(ExecResult::new(Vec::new(), Vec::new(), 0))
    }

    async fn running(&self) -> SandboxResult<bool> {
        Ok(true)
    }

    async fn ls(&self, path: &str, _user: AsUser) -> SandboxResult<Vec<FileEntry>> {
        self.listed.lock().expect("listed").push(path.to_owned());
        self.listings
            .get(path)
            .cloned()
            .ok_or_else(|| SandboxError::workspace_read_not_found(path))
    }

    async fn rm(&self, _path: &str, _recursive: bool, _user: AsUser) -> SandboxResult<()> {
        Ok(())
    }

    async fn mkdir(&self, path: &str, _parents: bool, _user: AsUser) -> SandboxResult<()> {
        self.calls
            .lock()
            .expect("calls")
            .push(Call::Mkdir(path.to_owned()));
        Ok(())
    }

    async fn read(&self, path: &str, _user: AsUser) -> SandboxResult<Vec<u8>> {
        Err(SandboxError::workspace_read_not_found(path))
    }

    async fn write(&self, path: &str, data: Vec<u8>, _user: AsUser) -> SandboxResult<()> {
        self.calls
            .lock()
            .expect("calls")
            .push(Call::Write(path.to_owned(), data));
        Ok(())
    }

    async fn persist_workspace(&self) -> SandboxResult<Vec<u8>> {
        Ok(Vec::new())
    }

    async fn hydrate_workspace(&self, _data: Vec<u8>) -> SandboxResult<()> {
        Ok(())
    }
}

/// Builds a tar out of members described as (name, kind, body).
fn archive(build: impl FnOnce(&mut tar::Builder<Vec<u8>>)) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    build(&mut builder);
    builder.into_inner().expect("archive")
}

/// Adds a member under a name the archive writer would otherwise refuse to produce.
fn member(
    builder: &mut tar::Builder<Vec<u8>>,
    name: &str,
    entry_type: tar::EntryType,
    body: &[u8],
) {
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(entry_type);
    header.set_size(body.len() as u64);
    header.set_mode(0o644);
    let raw = header.as_gnu_mut().expect("a gnu header");
    raw.name[..name.len()].copy_from_slice(name.as_bytes());
    header.set_cksum();
    builder.append(&header, body).expect("member");
}

/// Adds a link member aimed wherever the caller likes.
fn link_member(
    builder: &mut tar::Builder<Vec<u8>>,
    name: &str,
    entry_type: tar::EntryType,
    target: &str,
) {
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(entry_type);
    header.set_size(0);
    header.set_mode(0o777);
    builder
        .append_link(&mut header, name, target)
        .expect("member");
}

/// An ordinary two-file archive with a directory in it.
fn bundle() -> Vec<u8> {
    archive(|builder| {
        member(builder, "src/", tar::EntryType::Directory, b"");
        member(builder, "src/main.rs", tar::EntryType::Regular, b"fn main() {}");
        member(builder, "README.md", tar::EntryType::Regular, b"# bundle");
    })
}

#[tokio::test]
async fn an_archive_is_written_where_it_was_asked_and_unpacked_beside_itself() {
    let session = RecordingSession::new();
    let data = bundle();

    WorkspaceArchiveExtractor::new(&session)
        .extract(ARCHIVE_PATH, data.clone(), None, None)
        .await
        .expect("extract");

    // The archive itself lands first: it is what the caller handed over.
    assert_eq!(session.written(ARCHIVE_PATH), Some(data));
    assert_eq!(
        session.writes(),
        vec![
            ARCHIVE_PATH.to_owned(),
            "/workspace/incoming/src/main.rs".to_owned(),
            "/workspace/incoming/README.md".to_owned(),
        ]
    );
    assert_eq!(
        session.written("/workspace/incoming/src/main.rs"),
        Some(b"fn main() {}".to_vec())
    );
    // A directory member is created, and so is the parent of every file written.
    assert!(session.mkdirs().contains(&"/workspace/incoming/src".to_owned()));
}

#[tokio::test]
async fn members_named_from_the_archive_root_land_without_the_prefix() {
    // `tar -cf - .` names every member `./…`, which is what an archive produced by the shell looks
    // like. The leading `.` is part of naming the archive's own root, not part of the destination.
    let session = RecordingSession::new();
    let data = archive(|builder| {
        member(builder, "./", tar::EntryType::Directory, b"");
        member(builder, "./src/", tar::EntryType::Directory, b"");
        member(builder, "./src/main.rs", tar::EntryType::Regular, b"fn main() {}");
    });

    WorkspaceArchiveExtractor::new(&session)
        .extract(ARCHIVE_PATH, data, None, None)
        .await
        .expect("extract");

    assert_eq!(
        session.writes(),
        vec![
            ARCHIVE_PATH.to_owned(),
            "/workspace/incoming/src/main.rs".to_owned()
        ]
    );
    assert!(
        session
            .mkdirs()
            .contains(&"/workspace/incoming/src".to_owned())
    );
}

#[rstest]
#[case("/etc/passwd", "absolute path")]
#[case("../escape", "parent traversal")]
#[case("nested/../../escape", "parent traversal")]
#[case("c:/windows", "windows drive path")]
#[case("nested\\escape", "windows path separator")]
#[tokio::test]
async fn a_member_that_names_somewhere_else_is_refused(
    #[case] name: &str,
    #[case] reason: &str,
) {
    let session = RecordingSession::new();
    let data = archive(|builder| member(builder, name, tar::EntryType::Regular, b"payload"));

    let error = WorkspaceArchiveExtractor::new(&session)
        .extract(ARCHIVE_PATH, data, None, None)
        .await
        .expect_err("refused");

    assert_eq!(error.error_code(), ErrorCode::WorkspaceArchiveWriteError);
    assert_eq!(
        error.context().get("reason").and_then(|value| value.as_str()),
        Some(reason)
    );
    // The archive landed, because that is what the caller asked for; nothing was unpacked from it.
    assert_eq!(session.writes(), vec![ARCHIVE_PATH.to_owned()]);
    assert!(session.mkdirs().is_empty());
}

#[tokio::test]
async fn links_are_refused_in_both_directions() {
    for (entry_type, reason) in [
        (tar::EntryType::Symlink, "symlink member not allowed"),
        (tar::EntryType::Link, "hardlink member not allowed"),
    ] {
        let session = RecordingSession::new();
        let data = archive(|builder| link_member(builder, "link", entry_type, "/etc/passwd"));

        let error = WorkspaceArchiveExtractor::new(&session)
            .extract(ARCHIVE_PATH, data, None, None)
            .await
            .expect_err("refused");

        assert_eq!(
            error.context().get("reason").and_then(|value| value.as_str()),
            Some(reason)
        );
    }
}

#[tokio::test]
async fn a_member_that_is_neither_a_file_nor_a_directory_is_refused() {
    let session = RecordingSession::new();
    let data = archive(|builder| member(builder, "pipe", tar::EntryType::Fifo, b""));

    let error = WorkspaceArchiveExtractor::new(&session)
        .extract(ARCHIVE_PATH, data, None, None)
        .await
        .expect_err("refused");

    assert_eq!(
        error.context().get("reason").and_then(|value| value.as_str()),
        Some("unsupported member type")
    );
}

#[rstest]
#[case(tar::EntryType::Regular, "archive root member must be directory")]
#[case(tar::EntryType::Symlink, "archive root symlink")]
#[case(tar::EntryType::Link, "archive root hardlink")]
#[tokio::test]
async fn the_archives_own_root_may_only_be_a_directory(
    #[case] entry_type: tar::EntryType,
    #[case] reason: &str,
) {
    let session = RecordingSession::new();
    let data = archive(|builder| member(builder, ".", entry_type, b""));

    let error = WorkspaceArchiveExtractor::new(&session)
        .extract(ARCHIVE_PATH, data, None, None)
        .await
        .expect_err("refused");

    assert_eq!(
        error.context().get("reason").and_then(|value| value.as_str()),
        Some(reason)
    );
}

#[tokio::test]
async fn the_archives_own_root_directory_is_skipped_rather_than_written() {
    let session = RecordingSession::new();
    let data = archive(|builder| {
        member(builder, ".", tar::EntryType::Directory, b"");
        member(builder, "notes.md", tar::EntryType::Regular, b"kept");
    });

    WorkspaceArchiveExtractor::new(&session)
        .extract(ARCHIVE_PATH, data, None, None)
        .await
        .expect("extract");

    assert_eq!(
        session.writes(),
        vec![
            ARCHIVE_PATH.to_owned(),
            "/workspace/incoming/notes.md".to_owned()
        ]
    );
}

#[tokio::test]
async fn the_same_path_twice_is_refused_unless_both_are_directories() {
    let session = RecordingSession::new();
    let data = archive(|builder| {
        member(builder, "notes.md", tar::EntryType::Regular, b"first");
        member(builder, "notes.md", tar::EntryType::Regular, b"second");
    });

    let error = WorkspaceArchiveExtractor::new(&session)
        .extract(ARCHIVE_PATH, data, None, None)
        .await
        .expect_err("refused");

    assert_eq!(
        error.context().get("reason").and_then(|value| value.as_str()),
        Some("duplicate archive path: notes.md")
    );

    // Two directories are the same request made twice, which is not ambiguous.
    let session = RecordingSession::new();
    let data = archive(|builder| {
        member(builder, "src/", tar::EntryType::Directory, b"");
        member(builder, "src/", tar::EntryType::Directory, b"");
    });
    WorkspaceArchiveExtractor::new(&session)
        .extract(ARCHIVE_PATH, data, None, None)
        .await
        .expect("extract");
}

#[tokio::test]
async fn a_path_that_descends_through_a_file_is_refused_from_either_side() {
    // The file arrives first, and the member under it is the one that cannot be written.
    let session = RecordingSession::new();
    let data = archive(|builder| {
        member(builder, "src", tar::EntryType::Regular, b"a file");
        member(builder, "src/main.rs", tar::EntryType::Regular, b"under it");
    });
    let error = WorkspaceArchiveExtractor::new(&session)
        .extract(ARCHIVE_PATH, data, None, None)
        .await
        .expect_err("refused");
    assert_eq!(
        error.context().get("reason").and_then(|value| value.as_str()),
        Some("archive path descends through non-directory: src")
    );
    assert_eq!(
        error.context().get("member").and_then(|value| value.as_str()),
        Some("src/main.rs")
    );

    // The other order, where the conflict is only visible once the file turns up: the refusal names
    // the member that needed a directory, because that is the one that cannot be written.
    let session = RecordingSession::new();
    let data = archive(|builder| {
        member(builder, "src/main.rs", tar::EntryType::Regular, b"under it");
        member(builder, "src", tar::EntryType::Regular, b"a file");
    });
    let error = WorkspaceArchiveExtractor::new(&session)
        .extract(ARCHIVE_PATH, data, None, None)
        .await
        .expect_err("refused");
    assert_eq!(
        error.context().get("reason").and_then(|value| value.as_str()),
        Some("archive path descends through non-directory: src")
    );
    assert_eq!(
        error.context().get("member").and_then(|value| value.as_str()),
        Some("src/main.rs")
    );
}

#[tokio::test]
async fn a_member_landing_under_an_existing_symlink_is_refused() {
    // The workspace already holds `incoming/data` as a link. Writing `data/secret` through it would
    // land wherever the link points, which is not this workspace's decision to make.
    let session = RecordingSession::new().holding("/workspace/incoming", "data", EntryKind::Symlink);
    let data = archive(|builder| {
        member(builder, "data/secret", tar::EntryType::Regular, b"payload");
    });

    let error = WorkspaceArchiveExtractor::new(&session)
        .extract(ARCHIVE_PATH, data, None, None)
        .await
        .expect_err("refused");

    assert_eq!(
        error.context().get("reason").and_then(|value| value.as_str()),
        Some("symlink in parent path: data")
    );
    assert_eq!(session.writes(), vec![ARCHIVE_PATH.to_owned()]);
}

#[tokio::test]
async fn an_existing_directory_of_the_same_name_is_not_a_symlink() {
    let session =
        RecordingSession::new().holding("/workspace/incoming", "data", EntryKind::Directory);
    let data = archive(|builder| {
        member(builder, "data/secret", tar::EntryType::Regular, b"payload");
    });

    WorkspaceArchiveExtractor::new(&session)
        .extract(ARCHIVE_PATH, data, None, None)
        .await
        .expect("extract");

    assert!(
        session
            .writes()
            .contains(&"/workspace/incoming/data/secret".to_owned())
    );
}

#[tokio::test]
async fn an_archive_larger_than_the_caller_allows_is_refused_before_it_is_read() {
    let session = RecordingSession::new();
    let limits = SandboxArchiveLimits::default()
        .with_max_input_bytes(Some(16))
        .expect("limit");

    let error = WorkspaceArchiveExtractor::new(&session)
        .extract(ARCHIVE_PATH, bundle(), None, Some(limits))
        .await
        .expect_err("refused");

    assert_eq!(
        error.context().get("reason").and_then(|value| value.as_str()),
        Some("archive input size exceeds limit")
    );
    assert_eq!(
        error.context().get("limit").and_then(serde_json::Value::as_u64),
        Some(16)
    );
    // Not even the archive itself: it is bigger than this caller agreed to accept.
    assert!(session.calls().is_empty());
}

#[tokio::test]
async fn an_archive_with_more_members_than_the_caller_allows_is_refused() {
    let session = RecordingSession::new();
    let limits = SandboxArchiveLimits::default()
        .with_max_members(Some(2))
        .expect("limit");

    let error = WorkspaceArchiveExtractor::new(&session)
        .extract(ARCHIVE_PATH, bundle(), None, Some(limits))
        .await
        .expect_err("refused");

    assert_eq!(
        error.context().get("reason").and_then(|value| value.as_str()),
        Some("archive member count exceeds limit")
    );
    assert_eq!(
        error.context().get("actual").and_then(serde_json::Value::as_u64),
        Some(3)
    );
    assert_eq!(session.writes(), vec![ARCHIVE_PATH.to_owned()]);
}

#[tokio::test]
async fn an_archive_that_would_unpack_to_more_than_the_caller_allows_is_refused() {
    // Read from the headers, so a small archive declaring an enormous file is refused without any
    // of it being read.
    let session = RecordingSession::new();
    let limits = SandboxArchiveLimits::default()
        .with_max_extracted_bytes(Some(8))
        .expect("limit");

    let error = WorkspaceArchiveExtractor::new(&session)
        .extract(ARCHIVE_PATH, bundle(), None, Some(limits))
        .await
        .expect_err("refused");

    assert_eq!(
        error.context().get("reason").and_then(|value| value.as_str()),
        Some("archive extracted size exceeds limit")
    );
    assert_eq!(
        error.context().get("limit").and_then(serde_json::Value::as_u64),
        Some(8)
    );
}

#[tokio::test]
async fn a_caller_who_sets_no_limits_gets_none() {
    // The reference's default: `extract` without limits imposes nothing, and opting in is what
    // `SandboxArchiveLimits` is for.
    let session = RecordingSession::new();

    WorkspaceArchiveExtractor::new(&session)
        .extract(ARCHIVE_PATH, bundle(), None, None)
        .await
        .expect("extract");

    assert_eq!(session.writes().len(), 3);
}

#[test]
fn a_ceiling_of_zero_is_refused_where_it_is_written_down() {
    // Zero would refuse every archive, including an empty one, and reads as "unlimited" at a
    // glance.
    assert!(
        SandboxArchiveLimits::default()
            .with_max_input_bytes(Some(0))
            .is_err()
    );
    assert!(
        SandboxArchiveLimits::default()
            .with_max_extracted_bytes(Some(0))
            .is_err()
    );
    assert!(
        SandboxArchiveLimits::default()
            .with_max_members(Some(0))
            .is_err()
    );
    // `None` is how a caller turns one off.
    let unlimited = SandboxArchiveLimits::default()
        .with_max_members(None)
        .expect("limit");
    assert_eq!(unlimited.max_members(), None);
}

#[rstest]
#[case("/workspace/bundle.tar", None, "tar")]
#[case("/workspace/bundle.zip", None, "zip")]
// An explicit scheme wins over the name, which is how a caller unpacks `payload.bin`.
#[case("/workspace/payload.bin", Some(CompressionScheme::Tar), "tar")]
fn the_format_comes_from_the_name_unless_the_caller_says_otherwise(
    #[case] path: &str,
    #[case] given: Option<CompressionScheme>,
    #[case] expected: &str,
) {
    let scheme = given.or_else(|| {
        CompressionScheme::from_file_name(path.rsplit('/').next().expect("a name"))
    });
    assert_eq!(scheme.map(CompressionScheme::as_str), Some(expected));
}

#[rstest]
// A name with no extension says nothing about its format.
#[case("/workspace/bundle", "could not determine compression scheme")]
// `archive.tar.gz` names `gz`, which is a format this does not unpack — the reference reads only
// the last extension too.
#[case("/workspace/bundle.tar.gz", "compression scheme must be one of 'zip' 'tar'")]
#[tokio::test]
async fn an_archive_whose_format_cannot_be_read_is_refused(
    #[case] path: &str,
    #[case] message: &str,
) {
    let session = RecordingSession::new();

    let error = WorkspaceArchiveExtractor::new(&session)
        .extract(path, bundle(), None, None)
        .await
        .expect_err("refused");

    assert_eq!(error.error_code(), ErrorCode::InvalidCompressionScheme);
    assert_eq!(error.message(), message);
    // `test_extract_archive_rejects_missing_compression_scheme`: the path as given, and the
    // extension it named — none at all for a bare name.
    assert_eq!(
        error.context().get("path").and_then(|value| value.as_str()),
        Some(path)
    );
    let named = path.rsplit_once('.').map(|(_, suffix)| suffix);
    assert_eq!(
        error
            .context()
            .get("scheme")
            .and_then(|value| value.as_str()),
        named
    );
    assert!(session.calls().is_empty());
}

#[tokio::test]
async fn a_zip_archive_is_unpacked_beside_itself() {
    let session = RecordingSession::new();
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    writer.start_file("src/main.rs", zip::write::SimpleFileOptions::default()).expect("member");
    std::io::Write::write_all(&mut writer, b"fn main() {}").expect("body");
    let data = writer.finish().expect("archive").into_inner();
    WorkspaceArchiveExtractor::new(&session)
        .extract("/workspace/bundle.zip", data.clone(), None, None)
        .await
        .expect("extract");
    assert_eq!(session.written("/workspace/bundle.zip"), Some(data));
    assert_eq!(session.written("/workspace/src/main.rs"), Some(b"fn main() {}".to_vec()));
}

#[tokio::test]
async fn compressed_tar_formats_are_unpacked() {
    let original = bundle();
    let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    std::io::Write::write_all(&mut gzip, &original).expect("gzip body");
    let gzip = gzip.finish().expect("gzip");
    let mut bzip = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::default());
    std::io::Write::write_all(&mut bzip, &original).expect("bzip body");
    let bzip = bzip.finish().expect("bzip");
    let mut xz = lzma_rust2::XzWriter::new(Vec::new(), lzma_rust2::XzOptions::with_preset(6))
        .expect("xz writer");
    std::io::Write::write_all(&mut xz, &original).expect("xz body");
    let xz = xz.finish().expect("xz");
    for data in [gzip, bzip, xz] {
        let session = RecordingSession::new();
        WorkspaceArchiveExtractor::new(&session)
            .extract(ARCHIVE_PATH, data, None, None)
            .await
            .expect("extract");
        assert_eq!(
            session.written("/workspace/incoming/README.md"),
            Some(b"# bundle".to_vec())
        );
    }
}

#[tokio::test]
async fn zip_rejects_unsafe_paths_before_writing_members() {
    for name in ["../escape", "/absolute", "C:/drive", "dir\\file"] {
        let session = RecordingSession::new();
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        writer
            .start_file("safe.txt", zip::write::SimpleFileOptions::default())
            .expect("safe");
        std::io::Write::write_all(&mut writer, b"safe").expect("body");
        writer
            .start_file(name, zip::write::SimpleFileOptions::default())
            .expect("unsafe");
        std::io::Write::write_all(&mut writer, b"bad").expect("body");
        let data = writer.finish().expect("archive").into_inner();
        let error = WorkspaceArchiveExtractor::new(&session)
            .extract("/workspace/bundle.zip", data, None, None)
            .await
            .expect_err("unsafe member");
        assert_eq!(error.error_code(), ErrorCode::WorkspaceArchiveWriteError);
        assert_eq!(session.writes(), vec!["/workspace/bundle.zip"]);
    }
}

#[tokio::test]
async fn zip_checks_declared_size_before_writing_members() {
    let session = RecordingSession::new();
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    writer
        .start_file("large.txt", zip::write::SimpleFileOptions::default())
        .expect("member");
    std::io::Write::write_all(&mut writer, b"12345").expect("body");
    let data = writer.finish().expect("archive").into_inner();
    let limits = SandboxArchiveLimits::default()
        .with_max_extracted_bytes(Some(4))
        .expect("limit");
    let error = WorkspaceArchiveExtractor::new(&session)
        .extract("/workspace/bundle.zip", data, None, Some(limits))
        .await
        .expect_err("oversize");
    assert_eq!(error.error_code(), ErrorCode::WorkspaceArchiveWriteError);
    assert_eq!(session.writes(), vec!["/workspace/bundle.zip"]);
}

/// Compresses a tar the way a caller would hand it over as `.tar.gz`.
fn gzip(data: &[u8]) -> Vec<u8> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    std::io::Write::write_all(&mut encoder, data).expect("gzip body");
    encoder.finish().expect("gzip")
}

#[tokio::test]
async fn a_compressed_tar_over_the_extracted_size_limit_is_refused_before_writing_members() {
    let session = RecordingSession::new();
    // Compresses to a few hundred bytes, and declares far more than the ceiling allows.
    let data = gzip(&archive(|builder| {
        member(
            builder,
            "zeros.bin",
            tar::EntryType::Regular,
            &vec![0; 64 * 1024],
        );
    }));
    let limits = SandboxArchiveLimits::default()
        .with_max_extracted_bytes(Some(1024))
        .expect("limit");

    let error = WorkspaceArchiveExtractor::new(&session)
        .extract(ARCHIVE_PATH, data, None, Some(limits))
        .await
        .expect_err("oversize");

    assert_eq!(error.error_code(), ErrorCode::WorkspaceArchiveWriteError);
    assert_eq!(session.writes(), vec![ARCHIVE_PATH.to_owned()]);
}

#[tokio::test]
async fn a_compressed_tar_is_decoded_only_as_far_as_its_members_reach() {
    let session = RecordingSession::new();
    // A second gzip member that cannot be decoded follows the tar. Reading the stream to its end
    // would fail on it; the tar has already ended, so nothing needs to.
    let mut data = gzip(&bundle());
    data.extend_from_slice(&[0x1f, 0x8b, 0x08, 0x00, 0xde, 0xad, 0xbe, 0xef]);

    WorkspaceArchiveExtractor::new(&session)
        .extract(ARCHIVE_PATH, data, None, None)
        .await
        .expect("extract");

    assert_eq!(
        session.written("/workspace/incoming/README.md"),
        Some(b"# bundle".to_vec())
    );
}

/// A one-member zip whose headers claim the member is `declared` bytes, whatever it holds.
fn zip_with_declared_size(method: zip::CompressionMethod, body: &[u8], declared: u32) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    writer
        .start_file(
            "payload.bin",
            zip::write::SimpleFileOptions::default().compression_method(method),
        )
        .expect("member");
    std::io::Write::write_all(&mut writer, body).expect("body");
    let mut data = writer.finish().expect("archive").into_inner();
    // The uncompressed size sits at offset 22 of the local header and 24 of the central one.
    for (signature, offset) in [(b"PK\x03\x04", 22), (b"PK\x01\x02", 24)] {
        let start = data
            .windows(4)
            .position(|window| window == signature)
            .expect("header");
        data[start + offset..start + offset + 4].copy_from_slice(&declared.to_le_bytes());
    }
    data
}

#[tokio::test]
async fn a_zip_member_larger_than_its_header_declares_is_refused() {
    for method in [
        zip::CompressionMethod::Stored,
        zip::CompressionMethod::Deflated,
    ] {
        let session = RecordingSession::new();
        let data = zip_with_declared_size(method, &[b'x'; 4096], 1);
        // The declared size fits the ceiling; what the member really holds does not.
        let limits = SandboxArchiveLimits::default()
            .with_max_extracted_bytes(Some(16))
            .expect("limit");

        let error = WorkspaceArchiveExtractor::new(&session)
            .extract("/workspace/bundle.zip", data, None, Some(limits))
            .await
            .expect_err("size mismatch");

        assert_eq!(error.error_code(), ErrorCode::WorkspaceArchiveWriteError);
        assert!(
            format!("{error:?}").contains("does not match header"),
            "{method:?}: {error:?}"
        );
        assert_eq!(
            session.written("/workspace/payload.bin"),
            None,
            "{method:?}"
        );
    }
}

/// A one-member zip whose central directory says `made_by` wrote it, with these attributes.
fn zip_with_attributes(name: &str, made_by: u8, external_attributes: u32) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    writer
        .start_file(name, zip::write::SimpleFileOptions::default())
        .expect("member");
    std::io::Write::write_all(&mut writer, b"/etc/passwd").expect("body");
    let mut data = writer.finish().expect("archive").into_inner();
    // The system that made a member is the high byte of "version made by", at offset 5 of the
    // central header; the external attributes are at offset 38.
    let start = data
        .windows(4)
        .position(|window| window == b"PK\x01\x02")
        .expect("central header");
    data[start + 5] = made_by;
    data[start + 38..start + 42].copy_from_slice(&external_attributes.to_le_bytes());
    data
}

const ZIP_MADE_BY_DOS: u8 = 0;
const ZIP_MADE_BY_UNIX: u8 = 3;
const ZIP_SYMLINK_ATTRIBUTES: u32 = 0o120_777 << 16;

#[rstest]
#[case::made_on_unix(ZIP_MADE_BY_UNIX)]
// The `zip` crate reports a DOS-made member as a regular file whatever its high bits say. The
// reference reads the bits regardless, and so does this.
#[case::made_on_dos(ZIP_MADE_BY_DOS)]
#[tokio::test]
async fn a_zip_link_member_is_refused_whichever_system_made_it(#[case] made_by: u8) {
    let session = RecordingSession::new();
    let data = zip_with_attributes("link", made_by, ZIP_SYMLINK_ATTRIBUTES);

    let error = WorkspaceArchiveExtractor::new(&session)
        .extract("/workspace/bundle.zip", data, None, None)
        .await
        .expect_err("link member");

    assert_eq!(error.error_code(), ErrorCode::WorkspaceArchiveWriteError);
    assert_eq!(
        error
            .context()
            .get("reason")
            .and_then(|value| value.as_str()),
        Some("link member not allowed")
    );
    assert_eq!(session.writes(), vec!["/workspace/bundle.zip"]);
}

#[tokio::test]
async fn a_zip_member_that_is_a_link_and_a_climb_is_refused_as_the_climb() {
    let session = RecordingSession::new();
    let data = zip_with_attributes("../escape", ZIP_MADE_BY_UNIX, ZIP_SYMLINK_ATTRIBUTES);

    let error = WorkspaceArchiveExtractor::new(&session)
        .extract("/workspace/bundle.zip", data, None, None)
        .await
        .expect_err("unsafe member");

    assert_eq!(
        error
            .context()
            .get("reason")
            .and_then(|value| value.as_str()),
        Some("parent traversal")
    );
    assert_eq!(session.writes(), vec!["/workspace/bundle.zip"]);
}

/// A zip holding these members in this order; a name ending in `/` is a directory.
fn zip_of(members: &[(&str, &[u8])]) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for (name, body) in members {
        writer
            .start_file(*name, zip::write::SimpleFileOptions::default())
            .expect("member");
        std::io::Write::write_all(&mut writer, body).expect("body");
    }
    writer.finish().expect("archive").into_inner()
}

#[tokio::test]
async fn a_zip_path_given_twice_is_refused_unless_both_are_directories() {
    // Spelled differently, the same path once normalized; and a file and a directory that share
    // a name.
    for (first, second, duplicate) in [
        ("notes.md", "./notes.md", "notes.md"),
        ("src", "src/", "src"),
        ("src/", "src", "src"),
    ] {
        let session = RecordingSession::new();
        let data = zip_of(&[(first, b""), (second, b"")]);

        let error = WorkspaceArchiveExtractor::new(&session)
            .extract("/workspace/bundle.zip", data, None, None)
            .await
            .expect_err("refused");

        assert_eq!(
            error
                .context()
                .get("reason")
                .and_then(|value| value.as_str()),
            Some(format!("duplicate archive path: {duplicate}").as_str()),
            "{first} then {second}"
        );
        assert_eq!(session.writes(), vec!["/workspace/bundle.zip"]);
    }

    // Two directories are the same request made twice, which is not ambiguous.
    let session = RecordingSession::new();
    let data = zip_of(&[("src/", b""), ("src/./", b"")]);
    WorkspaceArchiveExtractor::new(&session)
        .extract("/workspace/bundle.zip", data, None, None)
        .await
        .expect("extract");
    assert!(session.mkdirs().contains(&"/workspace/src".to_owned()));
}

#[tokio::test]
async fn a_zip_path_that_descends_through_a_file_is_refused_from_either_side() {
    let orders: [[(&str, &[u8]); 2]; 2] = [
        [("src", b"a file"), ("src/main.rs", b"under it")],
        [("src/main.rs", b"under it"), ("src", b"a file")],
    ];
    for members in orders {
        let session = RecordingSession::new();
        let data = zip_of(&members);

        let error = WorkspaceArchiveExtractor::new(&session)
            .extract("/workspace/bundle.zip", data, None, None)
            .await
            .expect_err("refused");

        // Either way round, the member that cannot be written is the one under the file.
        assert_eq!(
            error
                .context()
                .get("reason")
                .and_then(|value| value.as_str()),
            Some("archive path descends through non-directory: src")
        );
        assert_eq!(
            error
                .context()
                .get("member")
                .and_then(|value| value.as_str()),
            Some("src/main.rs")
        );
        assert_eq!(session.writes(), vec!["/workspace/bundle.zip"]);
    }
}

/// A zip in which `renamed` is spelled `as_name` everywhere, so two entries share one name. The
/// writer refuses to produce that itself; an archive from elsewhere need not.
fn zip_renaming(members: &[(&str, &[u8])], renamed: &str, as_name: &str) -> Vec<u8> {
    assert_eq!(renamed.len(), as_name.len());
    let mut data = zip_of(members);
    let mut start = 0;
    while let Some(found) = data[start..]
        .windows(renamed.len())
        .position(|window| window == renamed.as_bytes())
    {
        let at = start + found;
        data[at..at + renamed.len()].copy_from_slice(as_name.as_bytes());
        start = at + renamed.len();
    }
    data
}

#[tokio::test]
async fn a_zip_naming_one_file_twice_is_refused_rather_than_unpacked_as_the_last() {
    // The `zip` crate keys members by name, so without a check of its own the second entry would
    // quietly replace the first.
    let session = RecordingSession::new();
    let data = zip_renaming(
        &[("notes.md", b"first"), ("notez.md", b"second")],
        "notez.md",
        "notes.md",
    );

    let error = WorkspaceArchiveExtractor::new(&session)
        .extract("/workspace/bundle.zip", data, None, None)
        .await
        .expect_err("refused");

    assert_eq!(
        error
            .context()
            .get("reason")
            .and_then(|value| value.as_str()),
        Some("duplicate archive path: notes.md")
    );
    assert_eq!(session.writes(), vec!["/workspace/bundle.zip"]);

    // One directory named twice is still the same request made twice.
    let session = RecordingSession::new();
    let data = zip_renaming(&[("src/", b""), ("srd/", b"")], "srd/", "src/");
    WorkspaceArchiveExtractor::new(&session)
        .extract("/workspace/bundle.zip", data, None, None)
        .await
        .expect("extract");
    assert!(session.mkdirs().contains(&"/workspace/src".to_owned()));
}

#[tokio::test]
async fn repeated_zip_directories_still_count_towards_member_limits() {
    let data = zip_renaming(&[("src/", b""), ("srd/", b"")], "srd/", "src/");
    for limit in [1, 2] {
        let session = RecordingSession::new();
        let limits = SandboxArchiveLimits::default()
            .with_max_members(Some(limit))
            .expect("limit");
        let result = WorkspaceArchiveExtractor::new(&session)
            .extract("/workspace/bundle.zip", data.clone(), None, Some(limits))
            .await;
        if limit == 1 {
            let error = result.expect_err("two entries exceed the member limit");
            assert_eq!(
                error.context().get("reason").and_then(|v| v.as_str()),
                Some("archive member count exceeds limit")
            );
            assert_eq!(
                error.context().get("actual").and_then(|v| v.as_u64()),
                Some(2)
            );
            assert!(session.mkdirs().is_empty());
        } else {
            result.expect("the exact limit allows both directory entries");
        }
    }
}

#[tokio::test]
async fn a_link_hidden_by_a_repeated_zip_directory_is_refused() {
    for made_by in [ZIP_MADE_BY_DOS, ZIP_MADE_BY_UNIX] {
        let mut data = zip_renaming(&[("src/", b""), ("srd/", b"")], "srd/", "src/");
        let first = data
            .windows(4)
            .position(|w| w == b"PK\x01\x02")
            .expect("header");
        data[first + 5] = made_by;
        data[first + 38..first + 42].copy_from_slice(&ZIP_SYMLINK_ATTRIBUTES.to_le_bytes());
        let session = RecordingSession::new();
        let error = WorkspaceArchiveExtractor::new(&session)
            .extract("/workspace/bundle.zip", data, None, None)
            .await
            .expect_err("hidden link");
        assert_eq!(
            error.context().get("reason").and_then(|v| v.as_str()),
            Some("link member not allowed")
        );
        assert_eq!(
            error.context().get("member").and_then(|v| v.as_str()),
            Some("src/")
        );
        assert!(session.mkdirs().is_empty());
        assert_eq!(session.writes(), vec!["/workspace/bundle.zip"]);
    }
}

#[tokio::test]
async fn repeated_zip_directories_still_count_all_declared_bytes() {
    let data = zip_renaming(&[("src/", b"1234"), ("srd/", b"567")], "srd/", "src/");
    for limit in [6, 7] {
        let session = RecordingSession::new();
        let limits = SandboxArchiveLimits::default()
            .with_max_extracted_bytes(Some(limit))
            .expect("limit");
        let result = WorkspaceArchiveExtractor::new(&session)
            .extract("/workspace/bundle.zip", data.clone(), None, Some(limits))
            .await;
        if limit == 6 {
            let error = result.expect_err("all original directory sizes count");
            assert_eq!(
                error.context().get("reason").and_then(|v| v.as_str()),
                Some("archive extracted size exceeds limit")
            );
            assert_eq!(
                error.context().get("actual").and_then(|v| v.as_u64()),
                Some(7)
            );
            assert!(session.mkdirs().is_empty());
        } else {
            result.expect("exact extracted size limit");
        }
    }
}

/// Encodes the first central record's size in ZIP64's extra field instead of its 32-bit field.
fn first_zip_size_as_zip64(mut data: Vec<u8>, size: u64) -> Vec<u8> {
    let start = data
        .windows(4)
        .position(|w| w == b"PK\x01\x02")
        .expect("central header");
    let name_len = usize::from(u16::from_le_bytes([data[start + 28], data[start + 29]]));
    let extra_len = u16::from_le_bytes([data[start + 30], data[start + 31]]);
    data[start + 24..start + 28].copy_from_slice(&u32::MAX.to_le_bytes());
    data[start + 30..start + 32].copy_from_slice(&(extra_len + 12).to_le_bytes());
    let mut extra = vec![1, 0, 8, 0];
    extra.extend_from_slice(&size.to_le_bytes());
    let at = start + 46 + name_len;
    data.splice(at..at, extra);
    let end = data
        .windows(4)
        .position(|w| w == b"PK\x05\x06")
        .expect("end record");
    let old = u32::from_le_bytes(data[end + 12..end + 16].try_into().expect("size field"));
    data[end + 12..end + 16].copy_from_slice(&(old + 12).to_le_bytes());
    data
}

#[tokio::test]
async fn a_collapsed_zip64_directory_uses_its_extended_size() {
    let data = first_zip_size_as_zip64(
        zip_renaming(&[("src/", b"12345"), ("srd/", b"")], "srd/", "src/"),
        5,
    );
    for limit in [4, 5] {
        let session = RecordingSession::new();
        let limits = SandboxArchiveLimits::default()
            .with_max_extracted_bytes(Some(limit))
            .expect("limit");
        let result = WorkspaceArchiveExtractor::new(&session)
            .extract("/workspace/bundle.zip", data.clone(), None, Some(limits))
            .await;
        if limit == 4 {
            let error = result.expect_err("hidden ZIP64 size exceeds limit");
            assert_eq!(
                error.context().get("actual").and_then(|v| v.as_u64()),
                Some(5)
            );
            assert!(session.mkdirs().is_empty());
        } else {
            result.expect("the extended size is five, not the 32-bit sentinel");
        }
    }
}

#[tokio::test]
async fn collapsed_zip_names_use_cp437_when_utf8_is_not_flagged() {
    let mut data = zip_renaming(&[("a/", b""), ("b/", b"")], "b/", "a/");
    let headers: Vec<_> = data
        .windows(4)
        .enumerate()
        .filter_map(|(at, signature)| match signature {
            b"PK\x03\x04" => Some((at, 6, 30)),
            b"PK\x01\x02" => Some((at, 8, 46)),
            _ => None,
        })
        .collect();
    for (at, flags, name) in headers {
        data[at + flags + 1] &= !8;
        data[at + name] = 0x82;
    }
    let session = RecordingSession::new();
    let limits = SandboxArchiveLimits::default()
        .with_max_members(Some(1))
        .expect("limit");
    let error = WorkspaceArchiveExtractor::new(&session)
        .extract("/workspace/bundle.zip", data.clone(), None, Some(limits))
        .await
        .expect_err("two entries");
    assert_eq!(
        error.context().get("member").and_then(|v| v.as_str()),
        Some("\u{e9}/")
    );
    WorkspaceArchiveExtractor::new(&session)
        .extract("/workspace/bundle.zip", data, None, None)
        .await
        .expect("valid CP437 names");
    assert!(session.mkdirs().contains(&"/workspace/\u{e9}".to_owned()));
}

#[tokio::test]
async fn collapsed_zip_root_entries_are_ignored_before_type_and_limit_checks() {
    let mut data = zip_renaming(&[(".", b"12345"), ("x", b"12345")], "x", ".");
    let first = data
        .windows(4)
        .position(|w| w == b"PK\x01\x02")
        .expect("header");
    data[first + 38..first + 42].copy_from_slice(&ZIP_SYMLINK_ATTRIBUTES.to_le_bytes());
    let limits = SandboxArchiveLimits::default()
        .with_max_members(Some(1))
        .expect("member limit")
        .with_max_extracted_bytes(Some(1))
        .expect("size limit");
    let session = RecordingSession::new();
    WorkspaceArchiveExtractor::new(&session)
        .extract("/workspace/bundle.zip", data, None, Some(limits))
        .await
        .expect("root entries skipped");
    assert!(session.mkdirs().is_empty());
    assert_eq!(session.writes(), vec!["/workspace/bundle.zip"]);
}

#[tokio::test]
async fn a_tar_split_across_concatenated_xz_streams_is_refused() {
    let session = RecordingSession::new();
    let original = bundle();
    // End the first stream inside a file's payload. Python's streaming tar reader uses a
    // single LZMADecompressor, so the second stream cannot complete this truncated member.
    let (head, tail) = original.split_at(1024 + 6);
    let mut data = Vec::new();
    for part in [head, tail] {
        let mut xz = lzma_rust2::XzWriter::new(Vec::new(), lzma_rust2::XzOptions::with_preset(6))
            .expect("xz writer");
        std::io::Write::write_all(&mut xz, part).expect("xz body");
        data.extend(xz.finish().expect("xz"));
    }

    let error = WorkspaceArchiveExtractor::new(&session)
        .extract(ARCHIVE_PATH, data.clone(), None, None)
        .await
        .expect_err("the first xz stream contains an incomplete tar");

    assert_eq!(error.error_code(), ErrorCode::WorkspaceArchiveWriteError);
    assert_eq!(session.written(ARCHIVE_PATH), Some(data));
    assert_eq!(session.writes(), vec![ARCHIVE_PATH.to_owned()]);
    assert!(session.mkdirs().is_empty());
}

#[tokio::test]
async fn a_tar_split_across_concatenated_gzip_members_is_refused() {
    let session = RecordingSession::new();
    let original = bundle();
    // End the first member inside a file's payload. Python's streaming tar reader uses a single
    // zlib decompressor, so the second member cannot complete this truncated one.
    let (head, tail) = original.split_at(1024 + 6);
    let mut data = gzip(head);
    data.extend(gzip(tail));

    let error = WorkspaceArchiveExtractor::new(&session)
        .extract(ARCHIVE_PATH, data.clone(), None, None)
        .await
        .expect_err("the first gzip member contains an incomplete tar");

    assert_eq!(error.error_code(), ErrorCode::WorkspaceArchiveWriteError);
    assert_eq!(session.written(ARCHIVE_PATH), Some(data));
    assert_eq!(session.writes(), vec![ARCHIVE_PATH.to_owned()]);
    assert!(session.mkdirs().is_empty());
}

/// A zip in which every occurrence of `from` is replaced by the same number of raw bytes, so a
/// name can hold what the writer would not put there itself.
fn zip_with_raw_bytes(members: &[(&str, &[u8])], patches: &[(&[u8], &[u8])]) -> Vec<u8> {
    let mut data = zip_of(members);
    for (from, to) in patches {
        assert_eq!(from.len(), to.len());
        let mut start = 0;
        while let Some(found) = data[start..]
            .windows(from.len())
            .position(|window| window == *from)
        {
            let at = start + found;
            data[at..at + to.len()].copy_from_slice(to);
            start = at + to.len();
        }
    }
    data
}

#[tokio::test]
async fn a_zip_name_ends_at_its_first_nul_as_the_reference_reads_it() {
    // Python's `ZipInfo` cuts a name at its first NUL. The `zip` crate keeps the rest, which would
    // otherwise reach the workspace as part of the path.
    let session = RecordingSession::new();
    let data = zip_with_raw_bytes(&[("notes.md", b"body")], &[(b"notes.md", b"note\0.md")]);

    WorkspaceArchiveExtractor::new(&session)
        .extract("/workspace/bundle.zip", data, None, None)
        .await
        .expect("extract");

    assert_eq!(session.written("/workspace/note"), Some(b"body".to_vec()));
    assert_eq!(
        session.writes(),
        vec!["/workspace/bundle.zip", "/workspace/note"]
    );
}

#[tokio::test]
async fn zip_names_that_differ_only_after_a_nul_are_the_same_path() {
    // Distinct to the `zip` crate, so nothing is folded away; the same path once cut at the NUL.
    let session = RecordingSession::new();
    let data = zip_with_raw_bytes(
        &[("axb", b"first"), ("axc", b"second")],
        &[(b"axb", b"a\0b"), (b"axc", b"a\0c")],
    );

    let error = WorkspaceArchiveExtractor::new(&session)
        .extract("/workspace/bundle.zip", data, None, None)
        .await
        .expect_err("refused");

    assert_eq!(
        error
            .context()
            .get("reason")
            .and_then(|value| value.as_str()),
        Some("duplicate archive path: a")
    );
    assert_eq!(session.writes(), vec!["/workspace/bundle.zip"]);
}

#[tokio::test]
async fn a_zip_name_flagged_utf8_that_is_not_utf8_is_refused() {
    // The reference fails to decode such a name and refuses the archive. The `zip` crate decodes it
    // lossily, into a name the archive never held, so the extractor must not take its word.
    let session = RecordingSession::new();
    let data = zip_with_raw_bytes(&[("\u{e9}.txt", b"body")], &[(b"\xc3\xa9", b"\xc3\x28")]);

    let error = WorkspaceArchiveExtractor::new(&session)
        .extract("/workspace/bundle.zip", data, None, None)
        .await
        .expect_err("refused");

    assert_eq!(error.error_code(), ErrorCode::WorkspaceArchiveWriteError);
    assert_eq!(
        error
            .context()
            .get("reason")
            .and_then(|value| value.as_str()),
        Some("unreadable archive")
    );
    assert_eq!(session.writes(), vec!["/workspace/bundle.zip"]);
}

/// The reason and member a refusal names, and what it measured when it was a limit.
fn refused_with(error: &ra_core::sandbox::SandboxError) -> (Option<&str>, Option<&str>) {
    (
        error
            .context()
            .get("reason")
            .and_then(|value| value.as_str()),
        error
            .context()
            .get("member")
            .and_then(|value| value.as_str()),
    )
}

/// `test_extract.py::test_extract_zip_rejects_member_count_over_limit` and
/// `…_zip_rejects_extracted_bytes_over_limit`: the member that crossed the limit, the limit and
/// what it came to, and nothing written but the archive.
#[tokio::test]
async fn a_zip_over_its_member_or_byte_limit_names_the_member_that_crossed_it() {
    let count = SandboxArchiveLimits::new()
        .with_max_input_bytes(None)
        .and_then(|limits| limits.with_max_extracted_bytes(None))
        .and_then(|limits| limits.with_max_members(Some(1)))
        .expect("limits");
    let bytes = SandboxArchiveLimits::new()
        .with_max_input_bytes(None)
        .and_then(|limits| limits.with_max_extracted_bytes(Some(4)))
        .and_then(|limits| limits.with_max_members(None))
        .expect("limits");
    for (data, limits, member, reason, limit, actual) in [
        (
            zip_of(&[("one.txt", b"1"), ("two.txt", b"2")]),
            count,
            "two.txt",
            "archive member count exceeds limit",
            1,
            2,
        ),
        (
            zip_of(&[("large.txt", b"12345")]),
            bytes,
            "large.txt",
            "archive extracted size exceeds limit",
            4,
            5,
        ),
    ] {
        let session = RecordingSession::new();

        let error = WorkspaceArchiveExtractor::new(&session)
            .extract("/workspace/bundle.zip", data, None, Some(limits))
            .await
            .expect_err("over the limit");

        assert_eq!(refused_with(&error), (Some(reason), Some(member)));
        assert_eq!(
            error
                .context()
                .get("limit")
                .and_then(serde_json::Value::as_u64),
            Some(limit)
        );
        assert_eq!(
            error
                .context()
                .get("actual")
                .and_then(serde_json::Value::as_u64),
            Some(actual)
        );
        assert_eq!(session.writes(), vec!["/workspace/bundle.zip"]);
    }
}

/// `test_extract.py::test_extract_zip_rejects_windows_drive_member_paths` and
/// `…_zip_rejects_windows_separator_member_paths`, with the reasons they name.
#[tokio::test]
async fn a_zip_member_named_for_windows_is_refused_for_the_reason_the_reference_gives() {
    for (name, reason) in [
        (r"C:\tmp\evil.txt", "windows drive path"),
        (r"\evil.txt", "windows path separator"),
    ] {
        let session = RecordingSession::new();

        let error = WorkspaceArchiveExtractor::new(&session)
            .extract(
                "/workspace/bundle.zip",
                zip_of(&[(name, b"evil")]),
                None,
                None,
            )
            .await
            .expect_err("refused");

        assert_eq!(refused_with(&error), (Some(reason), Some(name)));
        assert_eq!(session.writes(), vec!["/workspace/bundle.zip"]);
    }
}

/// `test_extract.py::test_extract_zip_rejects_symlinked_parent_paths`
#[tokio::test]
async fn a_zip_member_landing_under_an_existing_symlink_is_refused() {
    let session = RecordingSession::new().holding("/workspace", "link", EntryKind::Symlink);

    let error = WorkspaceArchiveExtractor::new(&session)
        .extract(
            "/workspace/bundle.zip",
            zip_of(&[("link/hello.txt", b"hello from zip")]),
            None,
            None,
        )
        .await
        .expect_err("refused");

    assert_eq!(
        refused_with(&error),
        (Some("symlink in parent path: link"), Some("link/hello.txt"))
    );
    assert_eq!(session.writes(), vec!["/workspace/bundle.zip"]);
}

/// `test_extract.py::test_extract_tar_reuses_directory_listings_during_symlink_checks`: each
/// directory on the way is listed once, however many members sit under it.
#[tokio::test]
async fn each_directory_is_listed_once_while_checking_for_links() {
    let session = RecordingSession::new();
    let data = archive(|builder| {
        member(builder, "nested/one.txt", tar::EntryType::Regular, b"one");
        member(builder, "nested/two.txt", tar::EntryType::Regular, b"two");
    });

    WorkspaceArchiveExtractor::new(&session)
        .extract("/workspace/bundle.tar", data, None, None)
        .await
        .expect("extract");

    assert_eq!(
        session.written("/workspace/nested/two.txt"),
        Some(b"two".to_vec())
    );
    assert_eq!(
        *session.listed.lock().expect("listed"),
        ["/workspace", "/workspace/nested"]
    );
}
