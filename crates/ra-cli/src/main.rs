//! The `ra` binary: initialize tracing, parse, dispatch, print.
//!
//! Everything else lives in the library beside it, so the command line can be exercised by the
//! test workspace — a binary's internals are reachable from nowhere.

use std::process::ExitCode;

use clap::Parser as _;

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt::init();
    match ra_cli::execute(ra_cli::Cli::parse()).await {
        Ok(output) => {
            print!("{}", output.stdout());
            output.outcome().exit_code()
        }
        Err(error) => {
            tracing::error!("{error:#}");
            ExitCode::FAILURE
        }
    }
}
