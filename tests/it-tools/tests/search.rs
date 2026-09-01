//! Contracts for the native `grep` and `glob` workspace tools.

use ra_core::{
    agent::AgentSpec,
    context::RunContext,
    item::{AgentId, CallId},
    state::RunId,
    tool::{Tool, ToolConcurrency, ToolContext, ToolOutput, TruncationStage},
};
use ra_tools::{
    glob::{GlobLimits, GlobTool},
    grep::{GrepLimits, GrepTool},
};
use serde_json::json;
use tempfile::TempDir;

fn run() -> RunContext {
    let agent = AgentSpec::builder()
        .id(AgentId::new("searcher"))
        .name("Searcher")
        .build()
        .expect("an agent");
    RunContext::new(RunId::new("run-search"), agent.as_ref())
}

async fn grep(tool: &GrepTool, arguments: serde_json::Value) -> ToolOutput {
    let run = run();
    tool.call(ToolContext::new(
        &run,
        tool,
        &CallId::new("call-grep"),
        &arguments,
    ))
    .await
    .expect("grep succeeds")
}

async fn glob(tool: &GlobTool, arguments: serde_json::Value) -> ToolOutput {
    let run = run();
    tool.call(ToolContext::new(
        &run,
        tool,
        &CallId::new("call-glob"),
        &arguments,
    ))
    .await
    .expect("glob succeeds")
}

/// The model-visible sentence a tool writes for a failure it shapes itself.
async fn grep_failure(tool: &GrepTool, arguments: serde_json::Value) -> String {
    let run = run();
    let call_id = CallId::new("call-grep-failure");
    let context = || ToolContext::new(&run, tool, &call_id, &arguments);
    let error = tool.call(context()).await.expect_err("grep fails");
    tool.handle_failure(&context(), &error)
        .await
        .expect("failure shaping succeeds")
        .expect("failure is model-visible")
        .as_text()
        .expect("text output")
        .to_owned()
}

fn grep_arguments(pattern: &str, path: Option<&str>) -> serde_json::Value {
    json!({
        "pattern": pattern,
        "path": path,
        "glob": null,
        "max_results": null,
        "case_insensitive": null,
    })
}

#[tokio::test]
async fn grep_returns_matches_and_scan_statistics() {
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::create_dir(workspace.path().join("src")).expect("src directory");
    std::fs::write(
        workspace.path().join("src/a.rs"),
        "fn main() {}\nfn helper() {}\n",
    )
    .expect("source file");
    std::fs::write(workspace.path().join("src/b.txt"), "no match\n").expect("text file");
    std::fs::write(workspace.path().join("image.bin"), [b'a', 0, b'b']).expect("binary file");
    let tool = GrepTool::rooted(workspace.path()).expect("rooted grep");

    tool.validate().expect("valid tool");
    assert_eq!(tool.options().concurrency(), ToolConcurrency::Parallel);
    assert_eq!(
        tool.model_definition().description(),
        Some(
            "Searches workspace text files with a regular expression. Results include `path:line` prefixes."
        )
    );
    let output = grep(
        &tool,
        json!({ "pattern": "fn", "path": null, "glob": "**/*.rs", "max_results": null, "case_insensitive": null }),
    )
    .await;

    assert_eq!(
        output.as_text().expect("text output"),
        "src/a.rs:1:fn main() {}\nsrc/a.rs:2:fn helper() {}\n2 matches (returned 2); scanned 1 files; skipped 0 binary, 0 over-size, and 0 unreadable files.\n"
    );
    assert!(output.metadata().truncations().is_empty());
    assert!(output.metadata().guidance().is_empty());
}

#[tokio::test]
async fn grep_reports_skips_and_a_narrowing_hint_when_truncated() {
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(
        workspace.path().join("matches.txt"),
        "hit one\nhit two\nhit three\n",
    )
    .expect("matches");
    std::fs::write(workspace.path().join("binary.dat"), [0, 1]).expect("binary");
    std::fs::write(workspace.path().join("large.txt"), "x".repeat(32)).expect("large");
    let tool = GrepTool::rooted(workspace.path())
        .expect("rooted grep")
        .with_limits(
            GrepLimits::new()
                .with_max_results(1)
                .with_max_file_bytes(28),
        );

    let output = grep(&tool, grep_arguments("hit", None)).await;

    let body = output.as_text().expect("text output");
    assert!(
        body.contains("3 matches (returned 1); scanned 1 files; skipped 1 binary, 1 over-size"),
        "{body}"
    );
    assert_eq!(output.metadata().truncations().len(), 1);
    assert_eq!(
        output.metadata().truncations()[0].stage(),
        TruncationStage::Tool
    );
    assert!(
        output
            .metadata()
            .guidance()
            .iter()
            .any(|item| item.contains("narrow"))
    );
    // An over-size file can be hiding a match; a binary one the tool never intended to read is not
    // news, and saying so on every call would make the sentence permanent noise.
    assert!(
        output
            .metadata()
            .guidance()
            .iter()
            .any(|item| item.contains("over-size or unreadable")),
        "{:?}",
        output.metadata().guidance()
    );
}

/// A truncation says how much was dropped, so it can never claim to have kept more than there was.
///
/// The summary sentence is part of the body and part of what a complete result would have cost, so
/// counting it on one side only is what turns a small truncated result into `72 of 18 bytes kept`.
#[tokio::test]
async fn a_truncated_search_never_retains_more_bytes_than_it_started_with() {
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(workspace.path().join("a.txt"), "hit\nhit\nhit\n").expect("matches");
    let grep_tool = GrepTool::rooted(workspace.path())
        .expect("rooted grep")
        .with_limits(GrepLimits::new().with_max_results(1));
    let glob_tool = GlobTool::rooted(workspace.path())
        .expect("rooted glob")
        .with_limits(GlobLimits::new().with_max_results(0));

    let outputs = [
        grep(&grep_tool, grep_arguments("hit", None)).await,
        glob(
            &glob_tool,
            json!({ "pattern": "**/*.txt", "path": null, "max_results": null }),
        )
        .await,
    ];

    for output in outputs {
        let truncations = output.metadata().truncations();
        assert_eq!(truncations.len(), 1, "{:?}", output.as_text());
        let truncation = &truncations[0];
        assert!(
            truncation.retained_bytes() < truncation.original_bytes(),
            "kept {} of {} bytes",
            truncation.retained_bytes(),
            truncation.original_bytes()
        );
        assert_eq!(
            truncation.retained_bytes(),
            output.as_text().expect("text output").len() as u64
        );
    }
}

/// A cut match line is bytes the model did not receive, so it is recorded as such.
#[tokio::test]
async fn grep_records_a_line_it_had_to_shorten() {
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(
        workspace.path().join("long.txt"),
        format!("needle{}\n", "x".repeat(4_000)),
    )
    .expect("long line");
    let tool = GrepTool::rooted(workspace.path()).expect("rooted grep");

    let output = grep(&tool, grep_arguments("needle", None)).await;

    let body = output.as_text().expect("text output");
    assert!(body.contains('…'), "{body}");
    assert_eq!(output.metadata().truncations().len(), 1);
    assert!(
        output
            .metadata()
            .guidance()
            .iter()
            .any(|item| item.contains("cut to fit")),
        "{:?}",
        output.metadata().guidance()
    );
}

#[tokio::test]
async fn glob_is_sorted_truncated_and_does_not_follow_external_links() {
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::create_dir(workspace.path().join("src")).expect("src directory");
    std::fs::write(workspace.path().join("src/z.rs"), "").expect("z source");
    std::fs::write(workspace.path().join("src/a.rs"), "").expect("a source");
    let outside = tempfile::tempdir().expect("outside directory");
    std::fs::write(outside.path().join("secret.rs"), "secret").expect("secret");
    #[cfg(unix)]
    std::os::unix::fs::symlink(outside.path(), workspace.path().join("linked-outside"))
        .expect("symlink");
    let tool = GlobTool::rooted(workspace.path())
        .expect("rooted glob")
        .with_limits(GlobLimits::new().with_max_results(1));

    tool.validate().expect("valid tool");
    assert_eq!(
        tool.model_definition().description(),
        Some("Finds files whose workspace-relative paths match a glob pattern.")
    );
    let output = glob(
        &tool,
        json!({ "pattern": "**/*.rs", "path": null, "max_results": null }),
    )
    .await;

    assert_eq!(
        output.as_text().expect("text output"),
        "src/a.rs\n2 files matched (returned 1); scanned 2 files; skipped 0 unreadable entries.\n"
    );
    assert_eq!(output.metadata().truncations().len(), 1);
}

/// `*` selects within one directory and `**` crosses them, so a pattern can name a single level.
///
/// globset's default is the opposite — `*` compiles to `.*` and matches a separator — which would
/// answer `src/*.rs` with the whole subtree and leave `**` meaning nothing the single star does not
/// already mean. Both tools compile their globs the same way, so the two agree on one pattern.
#[tokio::test]
async fn a_star_selects_one_directory_and_a_double_star_crosses_them() {
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::create_dir_all(workspace.path().join("src/deep")).expect("nested directories");
    std::fs::write(workspace.path().join("src/top.rs"), "needle\n").expect("top source");
    std::fs::write(workspace.path().join("src/deep/nested.rs"), "needle\n").expect("nested source");
    let tool = GlobTool::rooted(workspace.path()).expect("rooted glob");

    let shallow = glob(
        &tool,
        json!({ "pattern": "src/*.rs", "path": null, "max_results": null }),
    )
    .await;
    assert_eq!(
        shallow.as_text().expect("text output"),
        "src/top.rs\n1 files matched (returned 1); scanned 2 files; skipped 0 unreadable entries.\n"
    );

    let recursive = glob(
        &tool,
        json!({ "pattern": "src/**/*.rs", "path": null, "max_results": null }),
    )
    .await;
    assert_eq!(
        recursive.as_text().expect("text output"),
        "src/deep/nested.rs\nsrc/top.rs\n2 files matched (returned 2); scanned 2 files; skipped 0 unreadable entries.\n"
    );

    let grep_tool = GrepTool::rooted(workspace.path()).expect("rooted grep");
    let filtered = grep(
        &grep_tool,
        json!({ "pattern": "needle", "path": null, "glob": "src/*.rs", "max_results": null, "case_insensitive": null }),
    )
    .await;
    assert_eq!(
        filtered.as_text().expect("text output"),
        "src/top.rs:1:needle\n1 matches (returned 1); scanned 1 files; skipped 0 binary, 0 over-size, and 0 unreadable files.\n"
    );
}

/// The byte budget stops the report; it does not sift it.
///
/// Skipping a line that does not fit and taking a shorter one further down would leave the model
/// holding a subset assembled by length while the guidance tells it that it holds the first
/// `returned` results — and length is not something a narrower `path` or pattern lets it page
/// through.
#[tokio::test]
async fn a_full_report_keeps_a_prefix_rather_than_whatever_still_fits() {
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(workspace.path().join("a-long-file-name.rs"), "").expect("long name");
    std::fs::write(workspace.path().join("b.rs"), "").expect("short name");
    let tool = GlobTool::rooted(workspace.path())
        .expect("rooted glob")
        .with_limits(GlobLimits::new().with_max_output_bytes(8));

    let output = glob(
        &tool,
        json!({ "pattern": "**/*.rs", "path": null, "max_results": null }),
    )
    .await;

    // `b.rs\n` is five bytes and would have fit in the budget the first path overran.
    let body = output.as_text().expect("text output");
    assert_eq!(
        body,
        "2 files matched (returned 0); scanned 2 files; skipped 0 unreadable entries.\n"
    );
    assert_eq!(output.metadata().truncations().len(), 1);
}

/// A link that stays inside the workspace names an ordinary file, and `read_file` reads it. A
/// search that could not see it would disagree with the tool the model reaches for next.
#[cfg(unix)]
#[tokio::test]
async fn grep_searches_through_a_link_that_stays_inside_the_workspace() {
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(workspace.path().join("real.txt"), "needle here\n").expect("real file");
    std::os::unix::fs::symlink("real.txt", workspace.path().join("link.txt")).expect("symlink");
    let tool = GrepTool::rooted(workspace.path()).expect("rooted grep");

    let output = grep(&tool, grep_arguments("needle", None)).await;

    assert_eq!(
        output.as_text().expect("text output"),
        "link.txt:1:needle here\nreal.txt:1:needle here\n2 matches (returned 2); scanned 2 files; skipped 0 binary, 0 over-size, and 0 unreadable files.\n"
    );
}

/// Build and metadata trees are skipped because they are larger than the source beside them and
/// nobody greps them on purpose — but a search that names one has asked for it.
#[tokio::test]
async fn grep_skips_generated_trees_unless_the_path_names_one() {
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::create_dir(workspace.path().join("src")).expect("src directory");
    std::fs::create_dir(workspace.path().join("target")).expect("target directory");
    std::fs::create_dir(workspace.path().join(".git")).expect("git directory");
    std::fs::write(workspace.path().join("src/a.rs"), "needle\n").expect("source");
    std::fs::write(workspace.path().join("target/built.rs"), "needle\n").expect("generated");
    std::fs::write(workspace.path().join(".git/COMMIT_EDITMSG"), "needle\n").expect("git file");
    let tool = GrepTool::rooted(workspace.path()).expect("rooted grep");

    let unscoped = grep(&tool, grep_arguments("needle", None)).await;
    assert_eq!(
        unscoped.as_text().expect("text output"),
        "src/a.rs:1:needle\n1 matches (returned 1); scanned 1 files; skipped 0 binary, 0 over-size, and 0 unreadable files.\n"
    );

    let scoped = grep(&tool, grep_arguments("needle", Some("target"))).await;
    assert_eq!(
        scoped.as_text().expect("text output"),
        "target/built.rs:1:needle\n1 matches (returned 1); scanned 1 files; skipped 0 binary, 0 over-size, and 0 unreadable files.\n"
    );
}

/// A search told to look somewhere that is not there has not searched an empty directory.
#[tokio::test]
async fn a_search_path_that_does_not_exist_is_a_failure_rather_than_no_results() {
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(workspace.path().join("a.rs"), "needle\n").expect("source");
    let tool = GrepTool::rooted(workspace.path()).expect("rooted grep");

    let message = grep_failure(&tool, grep_arguments("needle", Some("crates/ra-codings"))).await;

    assert!(message.contains("No such directory or file"), "{message}");
}

/// Narrowing to one file is a legitimate way to spend a search.
#[tokio::test]
async fn a_search_path_may_name_a_single_file() {
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(workspace.path().join("a.rs"), "needle\n").expect("first source");
    std::fs::write(workspace.path().join("b.rs"), "needle\n").expect("second source");
    let tool = GrepTool::rooted(workspace.path()).expect("rooted grep");

    let output = grep(&tool, grep_arguments("needle", Some("b.rs"))).await;

    assert_eq!(
        output.as_text().expect("text output"),
        "b.rs:1:needle\n1 matches (returned 1); scanned 1 files; skipped 0 binary, 0 over-size, and 0 unreadable files.\n"
    );
}

/// One directory the walk cannot open costs its own contents, not the whole workspace's.
#[cfg(unix)]
#[tokio::test]
async fn an_unreadable_directory_is_counted_rather_than_fatal() {
    use std::os::unix::fs::PermissionsExt as _;

    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(workspace.path().join("a.rs"), "needle\n").expect("source");
    let closed = workspace.path().join("closed");
    std::fs::create_dir(&closed).expect("closed directory");
    std::fs::write(closed.join("hidden.rs"), "needle\n").expect("hidden source");
    std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o000))
        .expect("close the directory");
    // A test running as root can read it anyway, and then there is nothing to assert.
    let closed_to_us = std::fs::read_dir(&closed).is_err();
    let tool = GrepTool::rooted(workspace.path()).expect("rooted grep");

    let output = grep(&tool, grep_arguments("needle", None)).await;
    std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o700))
        .expect("reopen the directory");

    let body = output.as_text().expect("text output");
    assert!(body.contains("a.rs:1:needle"), "{body}");
    if closed_to_us {
        assert!(body.contains("and 1 unreadable files"), "{body}");
    }
}

#[tokio::test]
async fn search_tools_reject_paths_outside_the_workspace() {
    let workspace: TempDir = tempfile::tempdir().expect("workspace");
    let tool = GlobTool::rooted(workspace.path()).expect("rooted glob");
    let arguments = json!({ "pattern": "**", "path": "../outside", "max_results": null });
    let run = run();
    let call_id = CallId::new("call-glob-path");
    let context = || ToolContext::new(&run, &tool, &call_id, &arguments);
    let error = tool.call(context()).await.expect_err("outside path fails");
    let output = tool
        .handle_failure(&context(), &error)
        .await
        .expect("failure shaping succeeds")
        .expect("failure is model-visible");
    assert!(
        output
            .as_text()
            .expect("text output")
            .contains("does not resolve")
    );

    let grep_tool = GrepTool::rooted(workspace.path()).expect("rooted grep");
    let message = grep_failure(&grep_tool, grep_arguments("needle", Some("../outside"))).await;
    assert!(message.contains("does not resolve"), "{message}");
}
