//! `ra` --- the rusty-agent command-line entry point.
//!
//! This is the only crate that uses `anyhow`; library layers use their own `thiserror` errors.
//!
//! **The binary parses arguments and prints; it decides nothing about the product.** Which prefix a
//! dump covers, which tools a host-backed agent installs, and which roles exist are answered by
//! [`ra_coding::prompt::dump`], and the layering gate holds the binary to the product's public
//! entries rather than letting it assemble a report of its own. What is left here — flag names,
//! defaults, where output goes, and which exit code a drifted prefix earns — is genuinely the
//! command line's to choose.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::builder::PossibleValuesParser;
use clap::{Args, Parser, Subcommand};
use ra_coding::prompt::dump::{
    DEFAULT_ROLE, PromptDumpRequest, compare_prompt_dump, render_prompt_dump,
    render_prompt_dump_json, shipped_role_names,
};

/// Exit code for a `prompt dump --baseline` run whose prefix moved.
///
/// A drifted prefix is the answer to the question the flag asks, not a failure of the command, so
/// it is distinguished from the code a real error exits with.
const EXIT_PREFIX_CHANGED: u8 = 1;

/// How a command concluded, before it becomes a process exit code.
///
/// [`ExitCode`] is neither comparable nor constructible back into a number, so a command that only
/// ever returned one could not be tested on the thing scripts read. This is the testable form; the
/// binary is what turns it into a status.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandOutcome {
    /// The command did what was asked.
    Succeeded,
    /// `prompt dump --baseline` found the stable prefix had moved.
    PrefixChanged,
}

impl CommandOutcome {
    /// The process exit status this outcome reports.
    #[must_use]
    pub fn exit_code(self) -> ExitCode {
        match self {
            Self::Succeeded => ExitCode::SUCCESS,
            Self::PrefixChanged => ExitCode::from(EXIT_PREFIX_CHANGED),
        }
    }
}

/// What one command produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandOutput {
    stdout: String,
    outcome: CommandOutcome,
}

impl CommandOutput {
    fn new(stdout: impl Into<String>, outcome: CommandOutcome) -> Self {
        Self {
            stdout: stdout.into(),
            outcome,
        }
    }

    /// Text the command writes to standard output.
    #[must_use]
    pub fn stdout(&self) -> &str {
        &self.stdout
    }

    /// How the command concluded.
    #[must_use]
    pub const fn outcome(&self) -> CommandOutcome {
        self.outcome
    }
}

/// The `ra` command line.
#[derive(Parser, Debug)]
#[command(name = "ra", version, about = "rusty-agent")]
pub struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Runs one task.
    Run {
        /// Task description.
        prompt: String,
    },
    /// Inspects the assembled prompt.
    Prompt {
        #[command(subcommand)]
        command: PromptCommand,
    },
    /// Environment self-check: config / sandbox / prompt / mcp / provider.
    Doctor,
}

#[derive(Subcommand, Debug)]
enum PromptCommand {
    /// Prints the assembled stable prefix: sections, hashes, tokens, and cache plan.
    ///
    /// With `--baseline`, compares against a dump recorded earlier by `--json` and names the
    /// sections that moved the prefix hash. Exits 1 when the prefix changed.
    Dump(DumpArgs),
}

#[derive(Args, Debug)]
struct DumpArgs {
    /// Role whose prefix to report.
    #[arg(long, default_value = DEFAULT_ROLE, value_parser = PossibleValuesParser::new(shipped_role_names()))]
    role: String,

    /// Workspace a host-backed agent would open, whose installed tools the report names.
    #[arg(long, default_value = ".", conflicts_with = "no_tools")]
    workspace: PathBuf,

    /// Report the tool-free prefix instead, the one an agent built without a host carries.
    #[arg(long)]
    no_tools: bool,

    /// Label the report with a target provider. Assembly is provider-independent today.
    #[arg(long)]
    provider: Option<String>,

    /// Label the report with a target model. Assembly is model-independent today.
    #[arg(long)]
    model: Option<String>,

    /// Cache scope to record; defaults to a placeholder, since no run produced this report.
    #[arg(long)]
    cache_scope: Option<String>,

    /// Emit JSON, the form `--baseline` reads back.
    #[arg(long, conflicts_with = "baseline")]
    json: bool,

    /// Compare against a JSON dump recorded earlier instead of printing this one.
    #[arg(long, value_name = "PATH")]
    baseline: Option<PathBuf>,
}

impl DumpArgs {
    fn to_request(&self) -> PromptDumpRequest {
        let mut request = PromptDumpRequest::new().with_role(&self.role);
        if !self.no_tools {
            request = request.with_workspace(&self.workspace);
        }
        if let Some(provider) = &self.provider {
            request = request.with_provider(provider);
        }
        if let Some(model) = &self.model {
            request = request.with_model(model);
        }
        if let Some(scope) = &self.cache_scope {
            request = request.with_cache_scope(scope);
        }
        request
    }
}

/// Carries out one parsed command line and returns what it produced.
///
/// Returning the output rather than writing it is what lets the test workspace exercise a
/// subcommand end to end; the binary does the printing.
///
/// # Errors
///
/// Returns an error when the command could not be carried out. A `prompt dump --baseline` whose
/// prefix moved is not one of those: it is a [`CommandOutcome::PrefixChanged`] alongside the report
/// naming what moved.
pub fn execute(cli: Cli) -> anyhow::Result<CommandOutput> {
    match cli.command {
        Command::Run { prompt } => {
            tracing::info!(%prompt, "run 尚未实现");
            Ok(CommandOutput::new("", CommandOutcome::Succeeded))
        }
        Command::Prompt {
            command: PromptCommand::Dump(args),
        } => execute_prompt_dump(&args),
        Command::Doctor => {
            tracing::info!("doctor 尚未实现");
            Ok(CommandOutput::new("", CommandOutcome::Succeeded))
        }
    }
}

fn execute_prompt_dump(args: &DumpArgs) -> anyhow::Result<CommandOutput> {
    let request = args.to_request();

    let Some(baseline_path) = &args.baseline else {
        let report = if args.json {
            // The text report ends its own last line; serialized JSON does not, and a redirected
            // file without a trailing newline is a nuisance for everything that reads it back.
            format!("{}\n", render_prompt_dump_json(&request)?)
        } else {
            render_prompt_dump(&request)?
        };
        return Ok(CommandOutput::new(report, CommandOutcome::Succeeded));
    };

    let baseline = std::fs::read_to_string(baseline_path).map_err(|error| {
        anyhow::anyhow!(
            "cannot read baseline `{}`: {error}",
            baseline_path.display()
        )
    })?;
    let diff = compare_prompt_dump(&request, &baseline)?;
    let outcome = if diff.prefix_changed() {
        CommandOutcome::PrefixChanged
    } else {
        CommandOutcome::Succeeded
    };
    Ok(CommandOutput::new(diff.render_text(), outcome))
}
