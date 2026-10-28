//! The subset of the seven extension-safety rules that **a machine can check**.
//!
//! All seven are listed in the extension-safety section of the development plan. The ones
//! implemented here:
//!
//! | Rule | Check |
//! | ---: | --- |
//! | 1 | public data enums must be `#[non_exhaustive]` (with explicit upstream and control-flow exceptions) |
//! | 2 | public structs must have no public fields — public fields allow literal construction, so adding one breaks callers |
//! | grade | every crate's `lib.rs` must state `Stable` / `Evolving` / `Internal` |
//!
//! The other four (3 default implementations, 4 sealed traits, 5 open labels, 6 `schema_version`)
//! **depend on semantics rather than syntax**: which trait counts as "an extension point users
//! implement" and which enum counts as "a classification label" is something a machine cannot
//! tell, so a hard check would only manufacture false positives. They stay in the documentation
//! and in review until concrete extension traits make tighter checks useful. Better to miss than to
//! cry wolf — **a false positive makes people distrust the gate itself**.

use crate::source;

/// The valid stability levels.
const LEVELS: &[&str] = &["Stable", "Evolving", "Internal"];

/// The marker word for a stability grade. It matches source text, not gate output, so it followed
/// the switch to English `.rs` comments.
const GRADE_MARKER: &str = "**Stability**";

/// Checks one crate and returns its violations.
pub(crate) fn check(crate_name: &str) -> Vec<String> {
    let src = source::workspace_root()
        .join("crates")
        .join(crate_name)
        .join("src");
    let mut violations = Vec::new();

    violations.extend(check_stability_grade(crate_name, &src));

    for file in source::rust_files(&src) {
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        let where_ = source::relative(&file);
        match xtask::extension_safety_policy::check_source(&where_, &text) {
            Ok(found) => violations.extend(found),
            Err(error) => violations.push(format!("{where_}: {error}")),
        }
    }

    violations
}

/// Stability grade: every crate's `lib.rs` has to state what it promises.
fn check_stability_grade(crate_name: &str, src: &std::path::Path) -> Vec<String> {
    let lib = src.join("lib.rs");
    let Ok(text) = std::fs::read_to_string(&lib) else {
        return vec![]; // a binary crate has no lib.rs
    };

    let marked = text
        .lines()
        .filter(|line| line.starts_with("//!"))
        .any(|line| line.contains(GRADE_MARKER) && LEVELS.iter().any(|level| line.contains(level)));

    if marked {
        return vec![];
    }
    vec![format!(
        "{}：模块文档缺「{GRADE_MARKER}」标注（{}），\
         下游没法判断这个 crate 的公开面能不能依赖",
        source::relative(&lib),
        LEVELS.join(" / "),
    )]
    .into_iter()
    .map(|v| format!("{crate_name} {v}"))
    .collect()
}
