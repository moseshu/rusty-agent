//! 公开 API 快照：抽取与对账。
//!
//! # 为什么不是 `cargo public-api`
//!
//! 开发计划原本指定 `cargo public-api`。它更精确（走 rustdoc JSON，能解析
//! re-export 与 impl），但**要求 nightly**，而本仓库把工具链钉在 1.97.1 stable
//! （`rust-toolchain.toml`）。为一条门禁让 CI 装第二套工具链，代价大于收益。
//!
//! 这里改用 `syn` 直接扫源码。**精度确实不如**：`pub use` 再导出、`#[cfg]` 条件
//! 编译、泛型 impl 的展开都看不见。但门禁要抓的是「公开项被删了 / 签名变了 /
//! 悄悄多了一个」，这三件源码级扫描全都抓得到，够用。哪天真上了 nightly，换回
//! `cargo public-api` 只需替换本模块，基线格式与门禁逻辑不变。
//!
//! # 快照格式
//!
//! 每行一个公开项，`<种类> <路径><签名>`，按字典序排序。逐行 diff，因此
//! **行的顺序不构成契约，行的内容构成契约**。

use std::collections::BTreeSet;
use std::path::Path;

use quote::ToTokens;
use syn::{ImplItem, Item, Visibility};

use crate::source;

/// 抽取一个 crate 的公开 API 快照。
pub(crate) fn snapshot(crate_name: &str) -> Result<Vec<String>, String> {
    let src = source::workspace_root()
        .join("crates")
        .join(crate_name)
        .join("src");
    if !src.is_dir() {
        return Err(format!("找不到 {}", source::relative(&src)));
    }

    let root_module = crate_name.replace('-', "_");
    let mut items = BTreeSet::new();

    for file in source::rust_files(&src) {
        let text = std::fs::read_to_string(&file).map_err(|e| e.to_string())?;
        let parsed = syn::parse_file(&text)
            .map_err(|e| format!("{} 解析失败：{e}", source::relative(&file)))?;
        let module = module_path(&root_module, &src, &file);
        collect(&parsed.items, &module, &mut items);
    }

    Ok(items.into_iter().collect())
}

/// 文件路径 → 模块路径。`src/model/mod.rs` → `ra_core::model`。
fn module_path(root: &str, src: &Path, file: &Path) -> String {
    let Ok(relative) = file.strip_prefix(src) else {
        return root.to_owned();
    };
    let mut segments = vec![root.to_owned()];
    let components: Vec<String> = relative
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();

    for (index, component) in components.iter().enumerate() {
        let is_last = index + 1 == components.len();
        if !is_last {
            segments.push(component.clone());
            continue;
        }
        let stem = component.trim_end_matches(".rs");
        // lib.rs 与 mod.rs 不贡献一层路径。
        if stem != "lib" && stem != "mod" {
            segments.push(stem.to_owned());
        }
    }
    segments.join("::")
}

fn is_public(vis: &Visibility) -> bool {
    matches!(vis, Visibility::Public(_))
}

/// 拆签名：限定符、名字、名字之后的部分。
///
/// 不直接 dump 整个 `Signature`，因为那样会渲染成 `fn 路径::const fn 名字(...)`
/// ——`const` 跑到了路径后面。拆开重排，限定符归到行首。
fn sig_parts(sig: &syn::Signature) -> (String, String, String) {
    let mut kind = String::new();
    if sig.constness.is_some() {
        kind.push_str("const ");
    }
    if sig.asyncness.is_some() {
        kind.push_str("async ");
    }
    if sig.unsafety.is_some() {
        kind.push_str("unsafe ");
    }
    kind.push_str("fn");

    let generics = one_line(&sig.generics);
    let inputs = sig
        .inputs
        .iter()
        .map(one_line)
        .collect::<Vec<_>>()
        .join(", ");
    let output = match &sig.output {
        syn::ReturnType::Default => String::new(),
        ret @ syn::ReturnType::Type(..) => format!(" {}", one_line(ret)),
    };

    // where 子句是契约的一部分（放宽/收紧 bound 会影响下游），而 `Generics` 的
    // ToTokens 只渲染 `<...>`，得单独取。
    let where_clause = sig
        .generics
        .where_clause
        .as_ref()
        .map_or_else(String::new, |w| format!(" {}", one_line(w)));

    (
        kind,
        sig.ident.to_string(),
        format!("{generics}({inputs}){output}{where_clause}"),
    )
}

/// 渲染变体的字段部分。
///
/// **不渲染属性**：doc 注释是属性，把它写进快照会让每一次改注释都撞门禁——
/// 那会让人很快学会无脑 `--bless`，门禁也就废了。
fn variant_fields(variant: &syn::Variant) -> String {
    match &variant.fields {
        syn::Fields::Unit => String::new(),
        syn::Fields::Unnamed(fields) => {
            let types: Vec<String> = fields.unnamed.iter().map(|f| one_line(&f.ty)).collect();
            format!("({})", types.join(", "))
        }
        syn::Fields::Named(fields) => {
            let named: Vec<String> = fields
                .named
                .iter()
                .map(|f| {
                    let name = f
                        .ident
                        .as_ref()
                        .map_or_else(String::new, ToString::to_string);
                    format!("{name}: {}", one_line(&f.ty))
                })
                .collect();
            format!(" {{ {} }}", named.join(", "))
        }
    }
}

/// 把 token 流压成单行：签名里的换行与缩进不是契约。
fn one_line(tokens: impl ToTokens) -> String {
    let raw = tokens.to_token_stream().to_string();
    let mut out = String::with_capacity(raw.len());
    let mut prev_space = false;
    for ch in raw.chars() {
        if ch.is_whitespace() {
            if !prev_space {
                out.push(' ');
            }
            prev_space = true;
        } else {
            out.push(ch);
            prev_space = false;
        }
    }
    out.trim().to_owned()
}

fn collect(items: &[Item], module: &str, out: &mut BTreeSet<String>) {
    for item in items {
        match item {
            Item::Struct(s) if is_public(&s.vis) => {
                out.insert(format!("struct {module}::{}", s.ident));
                for field in &s.fields {
                    if is_public(&field.vis) {
                        let name = field
                            .ident
                            .as_ref()
                            .map_or_else(|| "0".to_owned(), ToString::to_string);
                        out.insert(format!(
                            "field {module}::{}.{name}: {}",
                            s.ident,
                            one_line(&field.ty)
                        ));
                    }
                }
            }
            Item::Enum(e) if is_public(&e.vis) => {
                out.insert(format!("enum {module}::{}", e.ident));
                for variant in &e.variants {
                    out.insert(format!(
                        "variant {module}::{}::{}{}",
                        e.ident,
                        variant.ident,
                        variant_fields(variant)
                    ));
                }
            }
            Item::Trait(t) if is_public(&t.vis) => {
                out.insert(format!("trait {module}::{}", t.ident));
                for trait_item in &t.items {
                    if let syn::TraitItem::Fn(f) = trait_item {
                        let (kind, name, rest) = sig_parts(&f.sig);
                        out.insert(format!("{kind} {module}::{}::{name}{rest}", t.ident));
                    }
                }
            }
            Item::Fn(f) if is_public(&f.vis) => {
                let (kind, name, rest) = sig_parts(&f.sig);
                out.insert(format!("{kind} {module}::{name}{rest}"));
            }
            Item::Const(c) if is_public(&c.vis) => {
                out.insert(format!("const {module}::{}: {}", c.ident, one_line(&c.ty)));
            }
            Item::Static(s) if is_public(&s.vis) => {
                out.insert(format!("static {module}::{}: {}", s.ident, one_line(&s.ty)));
            }
            Item::Type(t) if is_public(&t.vis) => {
                out.insert(format!("type {module}::{}", t.ident));
            }
            Item::Mod(m) if is_public(&m.vis) => {
                let inner = format!("{module}::{}", m.ident);
                out.insert(format!("mod {inner}"));
                if let Some((_, items)) = &m.content {
                    collect(items, &inner, out);
                }
            }
            Item::Impl(i) => collect_impl(i, module, out),
            _ => {}
        }
    }
}

fn collect_impl(item: &syn::ItemImpl, module: &str, out: &mut BTreeSet<String>) {
    // trait impl 不进快照：它由 trait 与类型共同决定，签名本身不是新增的公开面。
    if item.trait_.is_some() {
        return;
    }
    let self_ty = one_line(&item.self_ty);
    for impl_item in &item.items {
        match impl_item {
            ImplItem::Fn(f) if is_public(&f.vis) => {
                let (kind, name, rest) = sig_parts(&f.sig);
                out.insert(format!("{kind} {module}::{self_ty}::{name}{rest}"));
            }
            ImplItem::Const(c) if is_public(&c.vis) => {
                out.insert(format!(
                    "const {module}::{self_ty}::{}: {}",
                    c.ident,
                    one_line(&c.ty)
                ));
            }
            _ => {}
        }
    }
}

/// 基线文件路径。
pub(crate) fn baseline_path(crate_name: &str) -> std::path::PathBuf {
    source::workspace_root()
        .join("api")
        .join(format!("{crate_name}.txt"))
}

/// 读取基线；不存在时返回 `None`。
pub(crate) fn read_baseline(crate_name: &str) -> Option<Vec<String>> {
    let text = std::fs::read_to_string(baseline_path(crate_name)).ok()?;
    Some(
        text.lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .map(ToOwned::to_owned)
            .collect(),
    )
}

/// 写入基线（`--bless`）。
pub(crate) fn write_baseline(crate_name: &str, items: &[String]) -> Result<(), String> {
    let path = baseline_path(crate_name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let mut text = format!(
        "# {crate_name} 的公开 API 基线。由 `cargo xtask public-api --bless` 生成。\n\
         # 手改无意义——请改代码后重新生成，并让这份 diff 出现在 code review 里。\n"
    );
    for item in items {
        text.push_str(item);
        text.push('\n');
    }
    std::fs::write(&path, text).map_err(|e| e.to_string())
}
