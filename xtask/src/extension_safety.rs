//! 扩展安全七条里**能被机器检查**的那几条。
//!
//! 七条见开发计划「Rust 扩展安全七条」。这里落成检查的是：
//!
//! | 条 | 检查 |
//! | ---: | --- |
//! | ① | 公开数据枚举必须 `#[non_exhaustive]`（`NextStep` 是唯一例外） |
//! | ② | 公开结构体不得有公开字段——有公开字段就能字面量构造，加字段即破坏 |
//! | 分级 | 每个 crate 的 `lib.rs` 必须标注 `Stable` / `Evolving` / `Internal` |
//!
//! 另外四条（③ 默认实现、④ sealed、⑤ 开放标签、⑥ `schema_version`）**判据依赖
//! 语义而非语法**：哪个 trait 算「用户会实现的扩展点」、哪个枚举算「分类标签」，
//! 机器分不出来，硬检查只会制造误报。它们留在文档与 review 里，等 R1/R3 有了真实
//! trait 再看能不能收紧。宁可漏报也不误报——**误报会让人开始怀疑门禁本身**。

use syn::{Item, Visibility};

use crate::source;

/// 允许保持穷尽的公开枚举。
///
/// `NextStep` 是刻意的：它是控制流的唯一收口，框架自己 `match` 它，新增状态时
/// **必须**编译期报错。对外数据枚举与对内控制流枚举是两个方向的约束，不是矛盾。
const EXHAUSTIVE_ALLOWED: &[&str] = &["NextStep"];

/// 允许带公开字段的结构体。
///
/// 空表是目前的事实，也是希望维持的状态。真要加，得在这里留下名字与理由。
const PUBLIC_FIELDS_ALLOWED: &[&str] = &[];

/// 稳定性级别的合法取值。
const LEVELS: &[&str] = &["Stable", "Evolving", "Internal"];

/// 分级标注的标记词。`.rs` 注释统一英文后，这里跟着改；它匹配的是源码文本，不是输出。
const GRADE_MARKER: &str = "**Stability**";

/// 检查一个 crate，返回违规列表。
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
            continue; // 解析失败由 api 快照那边报，这里不重复
        };
        let where_ = source::relative(&file);
        check_items(&parsed.items, &where_, &mut violations);
    }

    violations
}

/// 分级标注：每个 crate 的 `lib.rs` 必须说清自己的稳定性承诺。
fn check_stability_grade(crate_name: &str, src: &std::path::Path) -> Vec<String> {
    let lib = src.join("lib.rs");
    let Ok(text) = std::fs::read_to_string(&lib) else {
        return vec![]; // 二进制 crate 没有 lib.rs
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
