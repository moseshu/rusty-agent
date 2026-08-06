//! `cargo xtask` —— 把开发计划里的硬约束变成 CI 门禁。
//!
//! 这些命令同时是 CI 检查项，见 `Docs/Rusty_Agent_Project_Structure.md` §5.6。

use clap::{Parser, Subcommand};

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    task: Task,
}

#[derive(Subcommand)]
enum Task {
    /// 断言工具 schema 连续渲染 100 次字节全同。
    SchemaStability,
    /// 导出各 provider 的 prompt 分段、hash、token、缓存断点。
    PromptDump,
    /// 校验 `Guard_Registry.md` 与代码一致，且硬阻断 guard <= 8。
    GuardRegistry,
    /// 校验工具 schema <= 20KB、各 prompt 段 token 上限。
    TokenBudget,
    /// 校验 crates/ 下没有 `#[cfg(test)]` 内联测试（测试一律在 tests/ workspace）。
    NoInlineTests,
    /// 运行独立测试 workspace：等价于 `cargo test --manifest-path tests/Cargo.toml`。
    Test,
}

fn main() {
    match Cli::parse().task {
        Task::SchemaStability => println!("schema-stability: 待实现"),
        Task::PromptDump => println!("prompt-dump: 待实现"),
        Task::GuardRegistry => println!("guard-registry: 待实现"),
        Task::TokenBudget => println!("token-budget: 待实现"),
        Task::NoInlineTests => println!("no-inline-tests: 待实现"),
        Task::Test => println!("test: 待实现"),
    }
}
