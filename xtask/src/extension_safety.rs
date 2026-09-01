//! The subset of the seven extension-safety rules that **a machine can check**.
//!
//! All seven are listed in the extension-safety section of the development plan. The ones
//! implemented here:
//!
//! | Rule | Check |
//! | ---: | --- |
//! | 1 | public data enums must be `#[non_exhaustive]` (`NextStep` is the sole exception) |
//! | 2 | public structs must have no public fields — public fields allow literal construction, so adding one breaks callers |
//! | grade | every crate's `lib.rs` must state `Stable` / `Evolving` / `Internal` |
//!
//! The other four (3 default implementations, 4 sealed traits, 5 open labels, 6 `schema_version`)
//! **depend on semantics rather than syntax**: which trait counts as "an extension point users
//! implement" and which enum counts as "a classification label" is something a machine cannot
//! tell, so a hard check would only manufacture false positives. They stay in the documentation
//! and in review until R1 and R3 provide real traits to tighten against. Better to miss than to
//! cry wolf — **a false positive makes people distrust the gate itself**.

use syn::{Item, Visibility};

use crate::source;

/// Public enums allowed to stay exhaustive.
///
/// `NextStep` is deliberate: it is the single point where control flow converges, the framework
/// `match`es it itself, and adding a state **must** be a compile error. An outward-facing data
/// enum and an inward-facing control-flow enum are constraints in opposite directions, not a
/// contradiction.
///
/// `ResolvedInstructions` is the same shape of decision. Its variants are the two positions an
/// agent's instructions may occupy in a model request — the cached prefix, or volatile tail
/// messages — and turn preparation `match`es it to decide which. A `_` arm would send a third
/// placement to whichever slot the arm happened to pick, which is the silent misplacement the type
/// was introduced to make impossible.
///
/// `SummarySlot` is a fixed external format, not an open classification. A tenth slot would make
/// an existing compaction summary no longer conform to its own durable format, so consumers must
/// be able to match all nine slots and a format change has to be deliberate and breaking.
const EXHAUSTIVE_ALLOWED: &[&str] = &["NextStep", "ResolvedInstructions", "SummarySlot"];

/// Structs allowed to have public fields.
///
/// An empty table is both the current fact and the desired state. Adding one means leaving a name
/// and a reason here.
const PUBLIC_FIELDS_ALLOWED: &[&str] = &[];

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
        let Ok(parsed) = syn::parse_file(&text) else {
            continue; // a parse failure is reported by the api snapshot; no need to repeat it
        };
        let where_ = source::relative(&file);
        check_items(&parsed.items, &where_, &mut violations);
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

fn check_items(items: &[Item], where_: &str, violations: &mut Vec<String>) {
    for item in items {
        match item {
            Item::Enum(e) if matches!(e.vis, Visibility::Public(_)) => {
                let name = e.ident.to_string();
                let has_attr = e
                    .attrs
                    .iter()
                    .any(|attr| attr.path().is_ident("non_exhaustive"));
                if !has_attr && !EXHAUSTIVE_ALLOWED.contains(&name.as_str()) {
                    violations.push(format!(
                        "{where_}：公开枚举 `{name}` 缺 `#[non_exhaustive]`（扩展安全 ①）——\
                         加一个变体就会让所有下游 match 编译失败。\
                         若它是框架自己收口的控制流枚举，加进 EXHAUSTIVE_ALLOWED 并写明理由"
                    ));
                }
            }
            Item::Struct(s) if matches!(s.vis, Visibility::Public(_)) => {
                let name = s.ident.to_string();
                if PUBLIC_FIELDS_ALLOWED.contains(&name.as_str()) {
                    continue;
                }
                for field in &s.fields {
                    if matches!(field.vis, Visibility::Public(_)) {
                        let field_name = field
                            .ident
                            .as_ref()
                            .map_or_else(|| "0".to_owned(), ToString::to_string);
                        violations.push(format!(
                            "{where_}：公开结构体 `{name}` 有公开字段 `{field_name}`（扩展安全 ②）——\
                             字面量构造一旦流行，加字段就是破坏性变更。用构造函数或 builder"
                        ));
                    }
                }
            }
            Item::Mod(m) => {
                if let Some((_, inner)) = &m.content {
                    check_items(inner, where_, violations);
                }
            }
            _ => {}
        }
    }
}
