//! `cargo xtask` —— 把开发计划里的硬约束变成 CI 门禁。
//!
//! 八条门禁（R0-6）。`cargo xtask all` 全跑一遍并给出汇总，**只有 FAIL 让 CI 变红**，
//! SKIP 单独计数——见 [`gate::Outcome`] 对三态的说明。
//!
//! ```text
//! cargo xtask all                # 全部
//! cargo xtask layering           # 依赖方向四条铁律
//! cargo xtask no-inline-tests    # crates/ 下无测试代码
//! cargo xtask test -p it-core    # 跑独立测试 workspace，参数透传给 cargo test
//! ```

mod gate;
mod inline_tests;
mod layering;
mod pending;
mod public_api;
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
    /// 跑全部八条门禁并汇总。
    All,
    /// 断言工具 schema 连续渲染 100 次字节全同。
    SchemaStability,
    /// 导出各 provider 的 prompt 分段、hash、token、缓存断点。
    PromptDump,
    /// 校验 `Guard_Registry.md` 与代码一致，且硬阻断 guard <= 8。
    GuardRegistry,
    /// 校验工具 schema <= 20KB、各 prompt 段 token 上限。
    TokenBudget,
    /// 校验 `crates/` 下没有任何测试代码（测试一律在 tests/ workspace）。
    NoInlineTests,
    /// 运行独立测试 workspace，额外参数透传给 `cargo test`。
    Test {
        /// 透传给 `cargo test` 的参数，如 `-p it-core`。
        ///
        /// `allow_hyphen_values` 必须开：否则 `-p` 会被 clap 当成本命令的选项，
        /// 逼着调用方写 `cargo xtask test -- -p it-core`。
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// 公开 API 与入库基线对账，未标注的破坏性变更失败。
    PublicApi,
    /// 依赖方向四条铁律：内核 / 可复用件 / 产品的依赖不得逆流。
    Layering,
}

/// 门禁清单。顺序即 `all` 的执行顺序：**先快后慢**，静态检查排在跑测试前面。
fn all_gates() -> Vec<(&'static str, Outcome)> {
    vec![
        ("layering", layering::run()),
        ("no-inline-tests", inline_tests::run()),
        ("public-api", public_api::run()),
        ("schema-stability", pending::schema_stability()),
        ("prompt-dump", pending::prompt_dump()),
        ("guard-registry", pending::guard_registry()),
        ("token-budget", pending::token_budget()),
        ("test", tests_workspace::run(&[])),
    ]
}

fn main() -> std::process::ExitCode {
    let outcomes = match Cli::parse().task {
        Task::All => all_gates(),
        Task::Layering => vec![("layering", layering::run())],
        Task::NoInlineTests => vec![("no-inline-tests", inline_tests::run())],
        Task::PublicApi => vec![("public-api", public_api::run())],
        Task::SchemaStability => vec![("schema-stability", pending::schema_stability())],
        Task::PromptDump => vec![("prompt-dump", pending::prompt_dump())],
        Task::GuardRegistry => vec![("guard-registry", pending::guard_registry())],
        Task::TokenBudget => vec![("token-budget", pending::token_budget())],
        Task::Test { args } => vec![("test", tests_workspace::run(&args))],
    };

    report(&outcomes)
}

/// 打印结果并给出退出码。
fn report(outcomes: &[(&str, Outcome)]) -> std::process::ExitCode {
    // 违规明细先打，免得被汇总表挤到屏幕外面。
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
