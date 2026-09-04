//! The `ra prompt dump` command line.
//!
//! The report's content is `ra-coding`'s to get right and is tested there. What is tested here is
//! the layer the user actually touches: which flags exist, what they default to, which pairs are
//! refused, and the exit status a script reads back.

use clap::{CommandFactory as _, Parser as _};
use ra_cli::{Cli, CommandOutcome, execute};

async fn run(args: &[&str]) -> ra_cli::CommandOutput {
    execute(Cli::parse_from(args))
        .await
        .expect("the command must carry out")
}

/// Clap's own consistency check: duplicate long flags, conflicts naming an argument that does not
/// exist, and defaults that fail their own value parser all panic here rather than at a user's
/// first invocation.
#[test]
fn test_the_command_line_is_internally_consistent() {
    Cli::command().debug_assert();
}

/// The default role must be one the product ships, and the flag must reject anything else at parse
/// time — a report for a role that does not exist would look exactly like a real one.
#[tokio::test]
async fn test_the_role_flag_offers_only_shipped_roles() {
    let defaulted = run(&["ra", "prompt", "dump", "--no-tools"]).await;
    let explicit = run(&["ra", "prompt", "dump", "--no-tools", "--role", "main"]).await;
    assert_eq!(
        defaulted.stdout(),
        explicit.stdout(),
        "the default role must be `main`"
    );

    for role in ra_coding::prompt::dump::shipped_role_names() {
        let parsed = Cli::try_parse_from(["ra", "prompt", "dump", "--role", role]);
        assert!(parsed.is_ok(), "`{role}` ships and must be accepted");
    }
    assert!(
        Cli::try_parse_from(["ra", "prompt", "dump", "--role", "archaeologist"]).is_err(),
        "an unknown role must be refused before any report is produced"
    );
}

/// `--no-tools` and `--workspace` describe two different agents. Accepting both would silently
/// report on one of them.
#[test]
fn test_the_two_prefix_variants_cannot_be_requested_at_once() {
    assert!(
        Cli::try_parse_from(["ra", "prompt", "dump", "--no-tools", "--workspace", "."]).is_err(),
        "`--no-tools` and `--workspace` must conflict"
    );
    assert!(
        Cli::try_parse_from([
            "ra",
            "prompt",
            "dump",
            "--json",
            "--baseline",
            "before.json"
        ])
        .is_err(),
        "`--json` prints a dump and `--baseline` compares one; asking for both is a mistake"
    );
}

/// The workspace default is the current directory, and the report names the tools an agent opened
/// there would install.
#[tokio::test]
async fn test_the_dump_defaults_to_the_prefix_an_agent_here_would_carry() {
    let here = run(&["ra", "prompt", "dump"]).await;
    assert!(
        here.stdout().contains("tool_surface"),
        "the default report covers the host-backed prefix, which advertises a tool surface"
    );

    let toolless = run(&["ra", "prompt", "dump", "--no-tools"]).await;
    assert!(
        !toolless.stdout().contains("tool_surface"),
        "`--no-tools` must report the prefix an agent built without a host carries"
    );
    assert_eq!(here.outcome(), CommandOutcome::Succeeded);
}

/// A workspace does not grant editing capability to a role that promises it has none.
///
/// The two halves are reported separately because they are different guarantees: a one-off role
/// carries no surface at all, while a read-only role carries one holding exactly the entries that
/// observe. Asserting the second as "no surface" is what previously hid a read-only specialist with
/// nothing to read.
#[tokio::test]
async fn test_a_host_backed_dump_keeps_one_off_roles_tool_free() {
    let report = run(&["ra", "prompt", "dump", "--role", "one_off_answer"]).await;
    assert!(
        !report.stdout().contains("tool_surface"),
        "a one-off role must not advertise host tools it cannot execute"
    );
}

/// A read-only role's report advertises what observes and nothing that writes.
#[tokio::test]
async fn test_a_host_backed_dump_leaves_read_only_roles_the_observing_entry() {
    for role in ["read_only_specialist", "planner"] {
        let report = run(&["ra", "prompt", "dump", "--role", role]).await;
        let stdout = report.stdout();
        assert!(
            stdout.contains("tool_surface"),
            "`{role}` must advertise the entries it did receive"
        );
        for withheld in ["apply_patch", "exec_command", "write_stdin"] {
            assert!(
                !stdout.contains(withheld),
                "`{role}` must not advertise `{withheld}`"
            );
        }
    }
}

/// Both label flags reach the report, and neither changes the prefix underneath it.
#[tokio::test]
async fn test_provider_and_model_label_the_report_without_changing_the_prefix() {
    let plain = run(&["ra", "prompt", "dump", "--no-tools"]).await;
    let labelled = run(&[
        "ra",
        "prompt",
        "dump",
        "--no-tools",
        "--provider",
        "anthropic",
        "--model",
        "claude-test",
    ])
    .await;

    assert!(labelled.stdout().contains("Provider: anthropic"));
    assert!(labelled.stdout().contains("Model:    claude-test"));

    let hash_line = |report: &str| {
        report
            .lines()
            .find(|line| line.starts_with("Stable Prefix Hash:"))
            .expect("every report states its prefix hash")
            .to_owned()
    };
    assert_eq!(
        hash_line(plain.stdout()),
        hash_line(labelled.stdout()),
        "no section is assembled per provider, so a label must not move the hash"
    );
}

/// The whole point of the flag: an unchanged prefix exits 0, a moved one exits 1 and names what
/// moved it. A script gating a release on prompt drift reads exactly this.
#[tokio::test]
async fn test_a_baseline_comparison_reports_drift_through_the_exit_status() {
    let workspace = tempfile::tempdir().expect("workspace");
    let baseline = workspace.path().join("before.json");
    let recorded = run(&["ra", "prompt", "dump", "--no-tools", "--json"]).await;
    std::fs::write(&baseline, recorded.stdout()).expect("baseline is writable");
    let baseline = baseline.to_str().expect("utf-8 path");

    let unchanged = run(&["ra", "prompt", "dump", "--no-tools", "--baseline", baseline]).await;
    assert_eq!(unchanged.outcome(), CommandOutcome::Succeeded);
    assert!(
        unchanged
            .stdout()
            .contains("Invalidated by:       nothing; cached prefixes survive")
    );

    // A different role is the cheapest real prefix change available from a command line: only the
    // role section differs, so the report has one honest trigger to name.
    let changed = run(&[
        "ra",
        "prompt",
        "dump",
        "--no-tools",
        "--role",
        "planner",
        "--baseline",
        baseline,
    ])
    .await;
    assert_eq!(changed.outcome(), CommandOutcome::PrefixChanged);
    assert!(changed.stdout().contains("Invalidated by:       role"));
}

/// A recorded dump must be JSON a later build can read back, not just text that happens to look
/// like it. Without this the comparison flag has no input.
#[tokio::test]
async fn test_the_recorded_dump_is_the_form_the_comparison_reads() {
    let recorded = run(&["ra", "prompt", "dump", "--no-tools", "--json"]).await;
    let value: serde_json::Value =
        serde_json::from_str(recorded.stdout()).expect("`--json` must emit JSON");

    assert!(value["prefix_hash"].is_string());
    assert!(value["sections"].is_array());
    assert!(
        value["cache_plan"]["cache_scope"].is_string(),
        "a recorded dump carries the placeholder scope, so its shape matches a run's"
    );
    assert!(
        recorded.stdout().ends_with('\n'),
        "a redirected dump must be a well-formed text file"
    );
}

/// A baseline that is not a dump has to say so. The failure mode this prevents is a comparison
/// against a half-read or unrelated file reporting every section as new.
#[tokio::test]
async fn test_an_unreadable_baseline_is_refused() {
    let workspace = tempfile::tempdir().expect("workspace");
    let junk = workspace.path().join("not-a-dump.json");
    std::fs::write(&junk, "{\"hello\": true}").expect("file is writable");

    let error = execute(Cli::parse_from([
        "ra",
        "prompt",
        "dump",
        "--no-tools",
        "--baseline",
        junk.to_str().expect("utf-8 path"),
    ]))
    .await
    .expect_err("a file that is not a dump must be refused");
    assert!(
        format!("{error:#}").contains("not a prompt dump"),
        "the message must say what was wrong with the file: {error:#}"
    );

    let missing = execute(Cli::parse_from([
        "ra",
        "prompt",
        "dump",
        "--no-tools",
        "--baseline",
        workspace
            .path()
            .join("absent.json")
            .to_str()
            .expect("utf-8 path"),
    ]))
    .await
    .expect_err("a baseline that is not there must be refused");
    assert!(format!("{missing:#}").contains("cannot read baseline"));
}
