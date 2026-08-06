//! `ra` —— rusty-agent 命令行入口。
//!
//! 这是唯一使用 `anyhow` 的 crate；库层一律用各自的 `thiserror` 错误。

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "ra", version, about = "rusty-agent")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 执行一次任务。
    Run {
        /// 任务描述。
        prompt: String,
    },
    /// 环境自检：config / sandbox / prompt / mcp / provider。
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
