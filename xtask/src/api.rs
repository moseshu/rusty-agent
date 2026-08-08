//! Public API snapshots: extraction and reconciliation.
//!
//! # Why not `cargo public-api`
//!
//! The development plan originally specified `cargo public-api`. It is more precise (it works from
//! rustdoc JSON and can resolve re-exports and impls) but **requires nightly**, while this
//! repository pins the toolchain to 1.97.1 stable (`rust-toolchain.toml`). Making CI install a
//! second toolchain for one gate costs more than it returns.
//!
//! This scans source directly with `syn` instead. **It is genuinely less precise**: `pub use`
//! re-exports, `#[cfg]` conditional compilation, and generic impl expansion are all invisible to
//! it. But what the gate has to catch is "a public item was deleted / a signature changed / one
//! quietly appeared", and a source-level scan catches all three, which is enough. If nightly ever
//! becomes acceptable, switching back to `cargo public-api` means replacing this module only —
//! the baseline format and the gate logic stay as they are.
//!
//! # Snapshot format
//!
//! One public item per line, `<kind> <path><signature>`, sorted lexically. The diff is line by
//! line, so **the order of lines is not a contract while their content is**.

use std::collections::BTreeSet;
use std::path::Path;

use quote::ToTokens;
use syn::{ImplItem, Item, Visibility};

use crate::source;

/// Extracts one crate's public API snapshot.
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

/// File path -> module path. `src/model.rs` -> `ra_core::model`.
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
        // `lib.rs` contributes no path segment, and `mod.rs` is skipped for the same reason:
        // `layering` already bans the old-style entry, so this branch is normally unreachable, but
        // if one does appear it should resolve to `foo` rather than `foo::mod` — otherwise
        // public-api would print a screen of unrelated baseline diff after layering already
        // reported the root cause.
        if stem != "lib" && stem != "mod" {
            segments.push(stem.to_owned());
        }
    }
    segments.join("::")
}

fn is_public(vis: &Visibility) -> bool {
    matches!(vis, Visibility::Public(_))
}

/// Splits a signature into qualifiers, name, and everything after the name.
///
/// Dumping the whole `Signature` directly would render as `fn <path>::const fn <name>(...)`, with
/// `const` stranded after the path. Splitting and reordering moves the qualifiers to the front of
/// the line.
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

    // A where clause is part of the contract (loosening or tightening a bound affects
    // downstream), but the `ToTokens` of `Generics` renders only `<...>`, so it is fetched
    // separately.
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

/// Renders the field portion of a variant.
///
/// **Attributes are not rendered**: a doc comment is an attribute, and putting it in the snapshot
/// would make every comment edit trip the gate — which teaches people to `--bless` reflexively,
/// and then the gate is worthless.
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

/// Flattens a token stream onto one line: newlines and indentation inside a signature are not a
/// contract.
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
    // A trait impl stays out of the snapshot: it is determined jointly by the trait and the type,
    // and its signature is not newly exposed surface.
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

/// Baseline file path.
pub(crate) fn baseline_path(crate_name: &str) -> std::path::PathBuf {
    source::workspace_root()
        .join("api")
        .join(format!("{crate_name}.txt"))
}

/// Reads the baseline; returns `None` when it does not exist.
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

/// Writes the baseline (`--bless`).
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
