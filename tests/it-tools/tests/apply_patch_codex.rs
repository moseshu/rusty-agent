// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! The patch tool against Codex's `apply_patch` writes: its scenario fixtures
//! (`apply-patch/tests/fixtures/scenarios`, copied to `fixtures/codex_apply_patch_scenarios`),
//! the overwrite and move cases of `apply-patch/tests/suite/tool.rs`, and the order Codex's
//! `apply_hunks_to_files` commits a move in, including `test_failed_move_returns_committed_destination_delta`.
//! The isolation cases are this port's own: its writer replaces entries instead of writing through
//! them.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use ra_core::{
    agent::AgentSpec,
    context::RunContext,
    event::{
        FileEvent, HostEventBody, HostEventSink, InMemoryHostEventSink,
        file::{FileChangeKind, FileChangedEvent},
    },
    item::{AgentId, CallId},
    state::{RunId, RunState},
    tool::{Tool, ToolContext, ToolServices},
};
use ra_exec::fs::RootedFileSystem;
use ra_tools::apply_patch::ApplyPatchTool;
use serde_json::json;
use tempfile::TempDir;

/// What one call wrote back: the model's text and the changes recorded on the host channel.
struct Applied {
    text: String,
    changes: Vec<FileChangedEvent>,
}

async fn apply(root: &Path, patch: &str) -> Applied {
    let filesystem = RootedFileSystem::open(root).expect("workspace capability");
    let tool = ApplyPatchTool::new(Arc::new(filesystem)).expect("tool");
    let sink = Arc::new(InMemoryHostEventSink::new());
    let services = ToolServices::new().with_event_sink(Arc::clone(&sink) as Arc<dyn HostEventSink>);
    let agent = AgentSpec::builder()
        .id(AgentId::new("patch-runner"))
        .name("Patch runner")
        .build()
        .expect("agent");
    let state = RunState::start(RunId::new("run-codex-patch"));
    let run = RunContext::new(RunId::new("run-codex-patch"), &agent)
        .with_event_seq_allocator(state.restore_event_seq_allocator(None));
    let call_id = CallId::new("call-codex-patch");
    let arguments = json!(patch);
    let context = || ToolContext::new(&run, &tool, &call_id, &arguments).with_services(&services);
    // A failed call is an error the tool renders itself, as the runtime shapes it for the model.
    let output = match tool.call(context()).await {
        Ok(output) => output,
        Err(error) => tool
            .handle_failure(&context(), &error)
            .await
            .expect("failure handling")
            .unwrap_or_else(|| panic!("the tool renders its own failure: {error}")),
    };
    let text = output.as_text().expect("text").to_owned();
    let changes = sink
        .events()
        .iter()
        .filter_map(|event| match event.body() {
            HostEventBody::File(FileEvent::Changed(change)) => Some(change.clone()),
            _ => None,
        })
        .collect();
    Applied { text, changes }
}

fn patch(body: &str) -> String {
    format!("*** Begin Patch\n{body}\n*** End Patch")
}

// --- apply-patch/tests/fixtures/scenarios ---------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
enum Entry {
    File(Vec<u8>),
    Dir,
}

fn snapshot(root: &Path) -> BTreeMap<PathBuf, Entry> {
    fn walk(root: &Path, dir: &Path, entries: &mut BTreeMap<PathBuf, Entry>) {
        for entry in std::fs::read_dir(dir).expect("readable directory") {
            let path = entry.expect("entry").path();
            let relative = path
                .strip_prefix(root)
                .expect("below the root")
                .to_path_buf();
            if path.is_dir() {
                entries.insert(relative, Entry::Dir);
                walk(root, &path, entries);
            } else {
                entries.insert(relative, Entry::File(std::fs::read(&path).expect("file")));
            }
        }
    }
    let mut entries = BTreeMap::new();
    if root.is_dir() {
        walk(root, root, &mut entries);
    }
    entries
}

fn copy_dir(from: &Path, to: &Path) {
    for entry in std::fs::read_dir(from).expect("readable input") {
        let path = entry.expect("entry").path();
        let target = to.join(path.file_name().expect("named"));
        if path.is_dir() {
            std::fs::create_dir_all(&target).expect("directory");
            copy_dir(&path, &target);
        } else {
            std::fs::copy(&path, &target).expect("file");
        }
    }
}

/// Scenarios whose patch text this port's V4A parser reads differently from Codex's parser. The
/// parser is not part of this port; each is listed with what differs.
const PARSER_DIFFERENCES: &[(&str, &str)] = &[];

/// Scenarios Codex runs through its standalone `apply_patch` binary, which applies a patch without
/// verifying it first, and that its tool handler would refuse at verification. The tool follows
/// the handler, so each of these must fail verification and leave the workspace as it was given.
const VERIFIED_BEFORE_APPLYING: &[(&str, &str)] = &[(
    "015_failure_after_partial_success_leaves_changes",
    "the patch deletes a file that does not exist, which `verify_apply_patch_args` refuses",
)];

/// Every scenario, run as Codex runs it: copy `input/`, apply `patch.txt`, compare with
/// `expected/` byte for byte, whatever the call reported.
#[tokio::test]
async fn codex_apply_patch_scenarios() {
    let scenarios =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/codex_apply_patch_scenarios");
    let mut names: Vec<_> = std::fs::read_dir(&scenarios)
        .expect("scenarios")
        .map(|entry| entry.expect("entry").path())
        .filter(|path| path.is_dir())
        .collect();
    names.sort();
    assert!(names.len() >= 24, "the scenarios are present: {names:?}");
    let mut failures = Vec::new();
    for scenario in &names {
        let name = scenario
            .file_name()
            .expect("named")
            .to_string_lossy()
            .into_owned();
        let workspace = TempDir::new().expect("workspace");
        let input = scenario.join("input");
        if input.is_dir() {
            copy_dir(&input, workspace.path());
        }
        let text = std::fs::read_to_string(scenario.join("patch.txt")).expect("patch");
        let before = snapshot(workspace.path());
        let applied = apply(workspace.path(), &text).await;
        if VERIFIED_BEFORE_APPLYING
            .iter()
            .any(|(verified, _)| *verified == name)
        {
            assert!(
                applied.text.starts_with("apply_patch verification failed:"),
                "{name}: {}",
                applied.text
            );
            assert_eq!(snapshot(workspace.path()), before, "{name} writes nothing");
            assert!(applied.changes.is_empty(), "{name} records nothing");
            continue;
        }
        if snapshot(workspace.path()) != snapshot(&scenario.join("expected")) {
            failures.push(format!("{name}: {}", applied.text));
        }
    }
    let expected: Vec<_> = PARSER_DIFFERENCES.iter().map(|(name, _)| *name).collect();
    let failed: Vec<_> = failures
        .iter()
        .map(|failure| failure.split(':').next().unwrap_or_default())
        .collect();
    assert_eq!(failed, expected, "scenario results: {failures:#?}");
}

// --- Overwrites (apply-patch/tests/suite/tool.rs) -------------------------------------------

#[tokio::test]
async fn an_added_file_overwrites_an_existing_one() {
    let workspace = TempDir::new().expect("workspace");
    std::fs::write(workspace.path().join("duplicate.txt"), "old content\n").expect("existing");

    let applied = apply(
        workspace.path(),
        &patch("*** Add File: duplicate.txt\n+new content"),
    )
    .await;

    assert_eq!(
        std::fs::read_to_string(workspace.path().join("duplicate.txt")).expect("file"),
        "new content\n",
        "{}",
        applied.text
    );
    assert_eq!(applied.changes.len(), 1);
    assert_eq!(applied.changes[0].change(), &FileChangeKind::Updated);
}

#[tokio::test]
async fn a_move_overwrites_its_destination() {
    let workspace = TempDir::new().expect("workspace");
    let root = workspace.path();
    std::fs::create_dir_all(root.join("old")).expect("source dir");
    std::fs::create_dir_all(root.join("renamed/dir")).expect("destination dir");
    std::fs::write(root.join("old/name.txt"), "from\n").expect("source");
    std::fs::write(root.join("renamed/dir/name.txt"), "existing\n").expect("destination");

    let applied = apply(
        root,
        &patch("*** Update File: old/name.txt\n*** Move to: renamed/dir/name.txt\n@@\n-from\n+new"),
    )
    .await;

    assert!(!root.join("old/name.txt").exists(), "{}", applied.text);
    assert_eq!(
        std::fs::read_to_string(root.join("renamed/dir/name.txt")).expect("destination"),
        "new\n"
    );
    assert_eq!(applied.changes.len(), 1, "one move: {:?}", applied.changes);
    assert_eq!(applied.changes[0].change(), &FileChangeKind::Moved);
    assert_eq!(applied.changes[0].moved_to(), Some("renamed/dir/name.txt"));
}

#[tokio::test]
async fn a_move_creates_missing_destination_directories() {
    let workspace = TempDir::new().expect("workspace");
    let root = workspace.path();
    std::fs::create_dir_all(root.join("old")).expect("source dir");
    std::fs::write(root.join("old/name.txt"), "old content\n").expect("source");

    apply(
        root,
        &patch(
            "*** Update File: old/name.txt\n*** Move to: renamed/dir/name.txt\n@@\n-old content\n+new content",
        ),
    )
    .await;

    assert!(!root.join("old/name.txt").exists());
    assert_eq!(
        std::fs::read_to_string(root.join("renamed/dir/name.txt")).expect("destination"),
        "new content\n"
    );
}

// --- Move commit order (apply-patch/src/lib.rs, apply_hunks_to_files) ------------------------

/// A destination that cannot be written leaves the source as it was: Codex writes the
/// destination before it touches the source.
#[tokio::test]
async fn a_move_whose_destination_cannot_be_written_changes_nothing() {
    for (case, prepare, destination) in [
        (
            "destination is a directory",
            (|root: &Path| std::fs::create_dir_all(root.join("dest")).expect("dir")) as fn(&Path),
            "dest",
        ),
        (
            "destination parent is a file",
            |root: &Path| std::fs::write(root.join("blocker"), "file\n").expect("blocker"),
            "blocker/dest.txt",
        ),
    ] {
        let workspace = TempDir::new().expect("workspace");
        let root = workspace.path();
        std::fs::write(root.join("src.txt"), "line\n").expect("source");
        prepare(root);
        let before = snapshot(root);

        let applied = apply(
            root,
            &patch(&format!(
                "*** Update File: src.txt\n*** Move to: {destination}\n@@\n-line\n+line2"
            )),
        )
        .await;

        assert_eq!(snapshot(root), before, "{case}: {}", applied.text);
        assert!(
            applied.text.starts_with("Patch was not applied:"),
            "{case}: {}",
            applied.text
        );
        assert!(applied.changes.is_empty(), "{case}: {:?}", applied.changes);
    }
}

/// Codex's `test_failed_move_returns_committed_destination_delta`: the source cannot be removed
/// after the destination was written, so the destination stays committed on its own and the
/// source keeps its contents.
#[cfg(unix)]
#[tokio::test]
async fn a_move_whose_source_cannot_be_removed_keeps_the_written_destination() {
    use std::os::unix::fs::PermissionsExt as _;

    for line_ending in ["\n", "\r\n", "\r"] {
        for existing in [false, true] {
            for (chunks, expected) in [
                ("\n@@\n-old\n+new", "head\nnew\ntail\n"),
                ("", "head\nold\ntail\n"),
            ] {
                let original = "head\nold\ntail\n".replace('\n', line_ending);
                let expected = expected.replace('\n', line_ending);
                let workspace = TempDir::new().expect("workspace");
                let root = workspace.path();
                std::fs::create_dir(root.join("locked")).expect("source dir");
                std::fs::create_dir(root.join("out")).expect("destination dir");
                std::fs::write(root.join("locked/src.txt"), &original).expect("source");
                if existing {
                    std::fs::write(root.join("out/dst.txt"), "previous\ncontents\n")
                        .expect("existing destination");
                }
                std::fs::set_permissions(
                    root.join("locked"),
                    std::fs::Permissions::from_mode(0o555),
                )
                .expect("read-only source dir");

                let applied = apply(
                    root,
                    &patch(&format!(
                        "*** Update File: locked/src.txt\n*** Move to: out/dst.txt{chunks}"
                    )),
                )
                .await;
                std::fs::set_permissions(
                    root.join("locked"),
                    std::fs::Permissions::from_mode(0o755),
                )
                .expect("restore");

                assert_eq!(
                    std::fs::read_to_string(root.join("locked/src.txt")).expect("source"),
                    original
                );
                assert_eq!(
                    std::fs::read_to_string(root.join("out/dst.txt")).expect("destination"),
                    expected
                );
                assert!(
                    applied.text.starts_with(
                        "Patch stopped after changing 1 path(s) (3 line(s) added, 0 line(s) removed):"
                    ) && applied
                        .text
                        .contains("failed to remove the original `locked/src.txt`"),
                    "{}",
                    applied.text
                );
                assert_eq!(applied.changes.len(), 1, "{:?}", applied.changes);
                let change = &applied.changes[0];
                assert_eq!(change.path(), "out/dst.txt");
                assert_eq!(
                    change.change(),
                    &if existing {
                        FileChangeKind::Updated
                    } else {
                        FileChangeKind::Added
                    }
                );
                assert_eq!((change.lines_added(), change.lines_removed()), (3, 0));
                assert_eq!(change.moved_to(), None);
            }
        }
    }
}

/// A move without changes is written at its destination and removed at its source, as Codex
/// does; a hard link to the source keeps the original file.
#[cfg(unix)]
#[tokio::test]
async fn a_pure_rename_writes_the_destination_and_removes_the_source() {
    let workspace = TempDir::new().expect("workspace");
    let root = workspace.path();
    std::fs::write(root.join("old.txt"), "same\n").expect("source");
    std::fs::hard_link(root.join("old.txt"), root.join("link.txt")).expect("hard link");

    apply(
        root,
        &patch("*** Update File: old.txt\n*** Move to: new.txt"),
    )
    .await;

    assert!(!root.join("old.txt").exists());
    assert_eq!(
        std::fs::read_to_string(root.join("new.txt")).expect("moved"),
        "same\n"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("link.txt")).expect("link"),
        "same\n"
    );
    // Written anew rather than renamed: the destination is not the source's inode.
    use std::os::unix::fs::MetadataExt as _;
    assert_ne!(
        std::fs::metadata(root.join("new.txt"))
            .expect("moved")
            .ino(),
        std::fs::metadata(root.join("link.txt"))
            .expect("link")
            .ino()
    );
}

/// A move onto its own path follows Codex's write-then-delete order and removes the file.
#[tokio::test]
async fn a_move_onto_its_own_path_follows_write_then_delete() {
    let workspace = TempDir::new().expect("workspace");
    let root = workspace.path();
    std::fs::write(root.join("same.txt"), "old\n").expect("file");

    let applied = apply(
        root,
        &patch("*** Update File: same.txt\n*** Move to: same.txt\n@@\n-old\n+new"),
    )
    .await;

    assert!(!root.join("same.txt").exists(), "{}", applied.text);
    assert!(
        applied.text.starts_with("Applied patch across 1 path(s)"),
        "{}",
        applied.text
    );
    assert_eq!(applied.changes.len(), 1);
    assert_eq!(applied.changes[0].path(), "same.txt");
    assert_eq!(applied.changes[0].change(), &FileChangeKind::Moved);
    assert_eq!(applied.changes[0].moved_to(), Some("same.txt"));
}

/// A later operation that cannot be written keeps the earlier ones (scenario 015's semantics).
#[tokio::test]
async fn a_later_write_failure_keeps_earlier_operations() {
    let workspace = TempDir::new().expect("workspace");
    let root = workspace.path();
    std::fs::write(root.join("blocker"), "file\n").expect("blocker");

    let applied = apply(
        root,
        &patch("*** Add File: ok.txt\n+ok\n*** Add File: blocker/new.txt\n+blocked"),
    )
    .await;

    assert_eq!(
        std::fs::read_to_string(root.join("ok.txt")).expect("ok"),
        "ok\n"
    );
    assert!(
        applied
            .text
            .starts_with("Patch stopped after changing 1 path(s)"),
        "{}",
        applied.text
    );
    assert_eq!(applied.changes.len(), 1);
    assert_eq!(applied.changes[0].path(), "ok.txt");
}

// --- Isolation kept by this port's writer ----------------------------------------------------

/// Overwriting through a link would write outside the granted paths; the link is replaced
/// instead, and the file it named is untouched.
#[cfg(unix)]
#[tokio::test]
async fn an_overwrite_replaces_a_symbolic_link_instead_of_writing_through_it() {
    let outside = TempDir::new().expect("outside");
    let victim = outside.path().join("victim.txt");
    std::fs::write(&victim, "outside\n").expect("victim");
    let workspace = TempDir::new().expect("workspace");
    let root = workspace.path();
    std::os::unix::fs::symlink(&victim, root.join("added.txt")).expect("add link");
    std::fs::write(root.join("src.txt"), "from\n").expect("source");
    std::os::unix::fs::symlink(&victim, root.join("moved.txt")).expect("move link");

    let applied = apply(
        root,
        &patch(
            "*** Add File: added.txt\n+added\n*** Update File: src.txt\n*** Move to: moved.txt\n@@\n-from\n+moved",
        ),
    )
    .await;

    assert_eq!(
        std::fs::read_to_string(&victim).expect("victim"),
        "outside\n",
        "{}",
        applied.text
    );
    for (name, content) in [("added.txt", "added\n"), ("moved.txt", "moved\n")] {
        let path = root.join(name);
        assert!(
            !std::fs::symlink_metadata(&path)
                .expect("entry")
                .file_type()
                .is_symlink(),
            "{name} is a file now"
        );
        assert_eq!(std::fs::read_to_string(&path).expect("file"), content);
    }
}

/// Overwriting a file that shares its inode with a file elsewhere leaves that other file as it
/// was.
#[cfg(unix)]
#[tokio::test]
async fn an_overwrite_does_not_write_through_a_hard_link() {
    let outside = TempDir::new().expect("outside");
    let victim = outside.path().join("victim.txt");
    std::fs::write(&victim, "outside\n").expect("victim");
    let workspace = TempDir::new().expect("workspace");
    let root = workspace.path();
    if std::fs::hard_link(&victim, root.join("added.txt")).is_err() {
        // Temporary directories on different filesystems cannot share an inode.
        return;
    }

    apply(root, &patch("*** Add File: added.txt\n+added")).await;

    assert_eq!(
        std::fs::read_to_string(&victim).expect("victim"),
        "outside\n"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("added.txt")).expect("file"),
        "added\n"
    );
}

#[tokio::test]
async fn an_added_file_cannot_replace_a_directory() {
    let workspace = TempDir::new().expect("workspace");
    let root = workspace.path();
    std::fs::create_dir(root.join("dir")).expect("dir");
    std::fs::write(root.join("dir/kept.txt"), "kept\n").expect("kept");

    let applied = apply(root, &patch("*** Add File: dir\n+file")).await;

    assert!(root.join("dir").is_dir(), "{}", applied.text);
    assert!(
        applied.text.starts_with("Patch was not applied:"),
        "{}",
        applied.text
    );
}

#[cfg(unix)]
#[tokio::test]
async fn an_overwritten_file_keeps_its_permissions() {
    use std::os::unix::fs::PermissionsExt as _;

    let workspace = TempDir::new().expect("workspace");
    let root = workspace.path();
    std::fs::write(root.join("script.sh"), "old\n").expect("file");
    std::fs::set_permissions(
        root.join("script.sh"),
        std::fs::Permissions::from_mode(0o750),
    )
    .expect("mode");

    apply(root, &patch("*** Add File: script.sh\n+new")).await;

    let mode = std::fs::metadata(root.join("script.sh"))
        .expect("file")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o750);
    assert_eq!(
        std::fs::read_to_string(root.join("script.sh")).expect("file"),
        "new\n"
    );
}
