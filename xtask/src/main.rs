//! `cargo xtask` --- turns the development plan's hard constraints into CI gates.
//!
//! Nine gates (R0-6). `cargo xtask all` runs them all and prints a summary; **only FAIL turns CI
//! red**, and SKIP is counted separately — see [`gate::Outcome`] on the three states.
//!
//! ```text
//! cargo xtask all                # everything
//! cargo xtask layering           # the four dependency-direction rules
//! cargo xtask no-inline-tests    # no test code under crates/
//! cargo xtask feature-matrix     # per crate, all features off / on
//! cargo xtask test -p it-core    # runs the separate test workspace, args pass through
//! ```

mod api;
mod extension_safety;
mod feature_matrix;
mod gate;
mod inline_tests;
mod layering;
mod pending;
mod prompt_dump;
mod public_api;
mod schema_stability;
mod source;
mod tests_workspace;

use clap::{Parser, Subcommand};

use gate::Outcome;

#[derive(Parser)]
#[command(about = "仓库门禁：把硬约束变成 CI 检查项")]
struct Cli {
    #[command(subcommand)]
    task: Task,
}

#[derive(Subcommand)]
enum Task {
    /// Runs all nine gates and summarizes.
    All,
    /// Reconciles actual provider tool payloads against their snapshot and asserts 100 renders.
    SchemaStability {
        /// Rewrites the snapshot after an intentional provider-wire schema change.
        #[arg(long)]
        bless: bool,
    },
    /// Reconciles the assembled stable prefix against its committed snapshot.
    PromptDump {
        /// Rewrites the snapshot instead of reconciling. Run it after an intentional prompt change
        /// so the diff lands in review.
        #[arg(long)]
        bless: bool,
    },
    /// Checks `Guard_Registry.md` against the code, and that hard-blocking guards number <= 8.
    GuardRegistry,
    /// Checks tool schemas are <= 20KB and each prompt section is within its token ceiling.
    TokenBudget,
    /// Checks there is no test code under `crates/` (tests all live in the tests/ workspace).
    NoInlineTests,
    /// Runs the separate test workspace, passing extra arguments through to `cargo test`.
    Test {
        /// Arguments passed through to `cargo test`, such as `-p it-core`.
        ///
        /// `allow_hyphen_values` has to be on: otherwise clap claims `-p` as an option of this
        /// command and forces callers to write `cargo xtask test -- -p it-core`.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// The public-surface contract: baseline reconciliation, extension safety 1 and 2, stability
    /// grades.
    PublicApi {
        /// Rewrites the baseline snapshot instead of reconciling. Run it after changing a public
        /// API so the diff lands in review.
        #[arg(long)]
        bless: bool,
    },
    /// The four dependency-direction rules: kernel, reusable pieces, and product may not depend
    /// upward.
    Layering,
    /// Per crate, checks that all features off and all features on both compile.
    FeatureMatrix,
}

/// The gate list. Its order is the execution order of `all`: **fast before slow**, with static
/// checks ahead of running tests.
fn all_gates() -> Vec<(&'static str, Outcome)> {
    vec![
        ("layering", layering::run()),
        ("no-inline-tests", inline_tests::run()),
        ("public-api", public_api::run(false)),
        ("schema-stability", schema_stability::run(false)),
        ("prompt-dump", prompt_dump::run(false)),
        ("guard-registry", pending::guard_registry()),
        ("token-budget", pending::token_budget()),
        ("feature-matrix", feature_matrix::run()),
        ("test", tests_workspace::run(&[])),
    ]
}

fn main() -> std::process::ExitCode {
    let outcomes = match Cli::parse().task {
        Task::All => all_gates(),
        Task::Layering => vec![("layering", layering::run())],
        Task::NoInlineTests => vec![("no-inline-tests", inline_tests::run())],
        Task::PublicApi { bless } => vec![("public-api", public_api::run(bless))],
        Task::SchemaStability { bless } => vec![("schema-stability", schema_stability::run(bless))],
        Task::PromptDump { bless } => vec![("prompt-dump", prompt_dump::run(bless))],
        Task::GuardRegistry => vec![("guard-registry", pending::guard_registry())],
        Task::TokenBudget => vec![("token-budget", pending::token_budget())],
        Task::FeatureMatrix => vec![("feature-matrix", feature_matrix::run())],
        Task::Test { args } => vec![("test", tests_workspace::run(&args))],
    };

    report(&outcomes)
}

/// Prints the results and sets the exit code.
fn report(outcomes: &[(&str, Outcome)]) -> std::process::ExitCode {
    // Violation detail comes first, so the summary table does not push it off screen.
    for (name, outcome) in outcomes {
        if let Outcome::Fail(violations) = outcome {
            println!("\n{name} 未通过：");
            for violation in violations {
                println!("  · {violation}");
            }
        }
    }

    println!();
    for (name, outcome) in outcomes {
        println!("{:<4}  {name:<17}{outcome}", outcome.status());
    }

    let failed = outcomes.iter().filter(|(_, o)| o.is_failure()).count();
    let skipped = outcomes.iter().filter(|(_, o)| o.is_skipped()).count();

    println!();
    if failed > 0 {
        println!("{failed} 条门禁未通过。");
        return std::process::ExitCode::FAILURE;
    }
    if skipped > 0 {
        println!("全部通过；另有 {skipped} 条待启用（被测对象尚未存在，见上表的任务号）。");
    } else {
        println!("全部通过。");
    }
    std::process::ExitCode::SUCCESS
}
