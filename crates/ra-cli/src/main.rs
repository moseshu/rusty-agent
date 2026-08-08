//! `ra` --- the rusty-agent command-line entry point.
//!
//! This is the only crate that uses `anyhow`; library layers use their own `thiserror` errors.

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "ra", version, about = "rusty-agent")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Runs one task.
    Run {
        /// Task description.
        prompt: String,
    },
    /// Environment self-check: config / sandbox / prompt / mcp / provider.
    Doctor,
}

fn main() {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();
    match cli.command {
        Command::Run { prompt } => {
            tracing::info!(%prompt, "run 尚未实现");
        }
        Command::Doctor => {
            tracing::info!("doctor 尚未实现");
        }
    }
}
