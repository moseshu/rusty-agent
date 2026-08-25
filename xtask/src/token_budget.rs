//! The `token-budget` gate: what the coding product's tool table costs a turn.
//!
//! # What it covers, and what it does not
//!
//! The tool half only, measured from both sides: the provider-neutral projection the profile
//! budgets, and the bytes a provider is actually handed, its own envelope included.
//! **Per-prompt-section token ceilings are not checked here**: no section declares one yet and
//! R4-7 owns building them, so a PASS says the advertised tool table fits — not that every prompt
//! section does.
//!
//! # Why it runs tests rather than measuring here
//!
//! The measure only means something on the same public assembly path an application uses, which
//! means constructing real tools; doing that in `xtask` would make it a second coding host. So the
//! product tests own the construction and this gate runs them.
//!
//! # Why each contract has to print a number
//!
//! **A zero exit code proves nothing about whether the assertion still exists.** A binary whose
//! budget case was deleted passes; so does a run whose name filter matched nothing, which reports
//! `0 passed` and exits 0 just like a real green. Either way a gate that only forwarded the exit
//! code would keep announcing a budget nobody measures — while adding nothing the `test` gate does
//! not already say. So each contract prints its measurement on a marked line, this gate quotes it,
//! and **a missing line is a failure**.

use std::{
    path::Path,
    process::{Command, Stdio},
};

use crate::{gate::Outcome, source};

/// One measured contract.
struct Contract {
    /// The file that owns it, named in failures so the reader lands on the assertion.
    file: &'static str,
    /// The test binary it lives in.
    binary: &'static str,
    /// The single case to run, when the binary also holds slower ones.
    case: Option<&'static str>,
    /// The marker its measurement comes out on. Kept in step with the constant of the same name
    /// in `file`.
    marker: &'static str,
    /// What it measures, for the failure line.
    subject: &'static str,
}

/// The provider-neutral half: what the tools that do the work cost of their share of the surface.
const SURFACE: Contract = Contract {
    file: "tests/it-coding/tests/tool_profile.rs",
    binary: "tool_profile",
    case: None,
    marker: "tool-schema-budget:",
    subject: "工具面 schema 预算",
};

/// The wire half: the same tools with each provider's envelope around them.
const WIRE: Contract = Contract {
    file: "tests/it-coding/tests/tool_schema_dump.rs",
    binary: "tool_schema_dump",
    // The rest of this binary renders the snapshot a hundred times per provider. Only the budget
    // case belongs to this gate; `prompt-dump` and `schema-stability` own the others.
    case: Some("test_current_provider_tool_tables_fit_the_coding_byte_budget"),
    marker: "tool-wire-budget:",
    subject: "provider wire 工具表预算",
};

impl Contract {
    /// Runs the contract and returns the measurement it printed.
    fn measure(&self, cargo: &str, root: &Path, manifest: &Path) -> Result<String, String> {
        let mut command = Command::new(cargo);
        command
            .args(["test", "--manifest-path"])
            .arg(manifest)
            .args(["-p", "it-coding", "--test", self.binary]);
        if let Some(case) = self.case {
            command.arg(case);
        }
        // `--nocapture` is what lets the measurement out of the harness; `--exact` keeps a case
        // name from selecting whatever else it happens to be a prefix of.
        command.args(["--", "--nocapture"]);
        if self.case.is_some() {
            command.arg("--exact");
        }

        // Only stdout is captured, because that is where the measurement comes out. stderr stays
        // inherited so a compile error or a failing assertion appears while it happens, rather
        // than being held back until the gate has made up its mind.
        let output = command
            .current_dir(root)
            .stdout(Stdio::piped())
            .spawn()
            .and_then(std::process::Child::wait_with_output)
            .map_err(|error| format!("无法启动{}契约：{error}", self.subject))?;
        let stdout = String::from_utf8_lossy(&output.stdout);

        if !output.status.success() {
            // The harness prints its failure summary on stdout, which was captured above; hand it
            // back rather than swallowing the one part that says which case failed.
            print!("{stdout}");
            let detail = match output.status.code() {
                Some(code) => format!("退出码 {code}"),
                None => "被信号中断".to_owned(),
            };
            return Err(format!(
                "{} 的{}未通过（{detail}）",
                self.file, self.subject
            ));
        }

        stdout
            .lines()
            .find_map(|line| line.trim().strip_prefix(self.marker))
            .map(|measurement| measurement.trim().to_owned())
            .ok_or_else(|| {
                format!(
                    "{} 全绿，却没有打印 `{}` 实测值——{}的断言已被改名或删除。\
                     按名字过滤、一条都没匹配上的 cargo test 同样退出 0，所以这里只能判红：\
                     放行的话，这条门禁就是在为一个没人量的预算报平安",
                    self.file, self.marker, self.subject
                )
            })
    }
}

/// Runs the gate.
pub(crate) fn run() -> Outcome {
    let root = source::workspace_root();
    let manifest = root.join("tests").join("Cargo.toml");
    if !manifest.is_file() {
        return Outcome::Fail(vec![format!(
            "{} 不存在，无法运行工具 schema 预算契约",
            source::relative(&manifest)
        )]);
    }

    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let surface = match SURFACE.measure(&cargo, &root, &manifest) {
        Ok(measurement) => measurement,
        Err(violation) => return Outcome::Fail(vec![violation]),
    };
    let wire = match WIRE.measure(&cargo, &root, &manifest) {
        Ok(measurement) => measurement,
        Err(violation) => return Outcome::Fail(vec![violation]),
    };

    Outcome::pass(format!(
        "已实现干活工具 schema 合计 {surface} 字节（份额由 20 KiB 工具面上限按声明项数均摊）；\
         provider wire 工具表最大 {wire} 字节（含各家 envelope）"
    ))
}
