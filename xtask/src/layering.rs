//! The `layering` gate: a direct-dependency allowlist, the four dependency-direction rules, and
//! the modern module layout.
//!
//! The rules come from the crate table in
//! [the development plan](../../Docs/Rusty_Agent_Framework_Development_Plan.md) and from
//! [the project structure](../../Docs/Rusty_Agent_Project_Structure.md) §1:
//!
//! 1. every crate may depend only on the internal crates its responsibility table allows;
//! 2. the **kernel** (`ra-core`, `ra-macros`, `ra-runtime`) depends on no reusable piece and no
//!    product;
//! 3. **reusable pieces** do not depend on products;
//! 4. **products depend on no other product**, and no framework crate references a product
//!    implementation;
//! 5. **no framework crate branches on a product name** (R18-8).
//!
//! The first four inspect the dependency graph (**including transitive edges** — `ra-core -> X ->
//! ra-coding` is equally a violation, and checking direct dependencies alone would miss it); the
//! fifth inspects source.
//!
//! The layer table is an **allowlist**: creating an unregistered crate fails the gate outright.
//! That is deliberate — which layer a new crate belongs to is a decision that has to be made on
//! the spot, not deferred.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::process::Command;

use crate::gate::Outcome;
use crate::source;
use xtask::layering_policy::{ALLOW_MARKER, scan_product_references};

/// Which layer a crate belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Layer {
    /// Kernel: framework machinery with zero business content.
    Kernel,
    /// General services: providers, sandboxes, sessions, and so on.
    Service,
    /// Reusable pieces: unchanged across businesses, needed by every product.
    Reusable,
    /// The reference product: business content.
    Product,
    /// Binary entry point. As the assembler, it may depend on products.
    Binary,
}

impl Layer {
    const fn label(self) -> &'static str {
        match self {
            Self::Kernel => "内核",
            Self::Service => "服务",
            Self::Reusable => "可复用件",
            Self::Product => "产品",
            Self::Binary => "二进制",
        }
    }

    /// Whether it belongs to the framework, as opposed to product content or the assembler.
    const fn is_framework(self) -> bool {
        matches!(self, Self::Kernel | Self::Service | Self::Reusable)
    }
}

/// The layer allowlist. The target shape is 17 crates; the ones not yet created (`ra-tools`,
/// `ra-flow`, `ra-assistant`) are registered in advance so the gate binds them the day they
/// appear.
const LAYERS: &[(&str, Layer)] = &[
    ("ra-core", Layer::Kernel),
    ("ra-macros", Layer::Kernel),
    ("ra-runtime", Layer::Kernel),
    ("ra-model", Layer::Service),
    ("ra-prompt", Layer::Service),
    ("ra-context", Layer::Service),
    ("ra-exec", Layer::Service),
    ("ra-session", Layer::Service),
    ("ra-mcp", Layer::Service),
    ("ra-protocol", Layer::Service),
    ("ra-eval", Layer::Service),
    ("ra-tools", Layer::Reusable),
    ("ra-flow", Layer::Reusable),
    ("ra-patch", Layer::Reusable),
    ("ra-coding", Layer::Product),
    ("ra-assistant", Layer::Product),
    ("ra-cli", Layer::Binary),
    ("xtask", Layer::Binary),
];

/// The direct internal dependencies each crate's responsibility table allows.
///
/// Layer rules catch an obvious inversion but not an edge like `ra-core -> ra-model`, where both
/// sides are framework yet the responsibilities are already backwards. This turns the development
/// plan's crate table into an allowlist; a new dependency has to answer why it belongs on that
/// boundary first. The three not-yet-created crates are pre-registered and bound from day one.
///
/// **This table is the single source of truth for the dependency graph**; the graph in
/// [the project structure](../../Docs/Rusty_Agent_Project_Structure.md) §1 has to agree with it.
/// When they disagree this wins and the document is fixed on the spot — the whole point of an
/// allowlist is to eliminate the drift where the diagram says one thing and `Cargo.toml` another.
///
/// # `ra-runtime` depending only on `ra-core` is a promise, not an omission
///
/// The loop kernel does not know `ra-model`, `ra-prompt`, or `ra-context`. It can only reach a
/// model through a trait in `ra-core`, and prompt assembly and context compaction have to be
/// injected by the assembly layer. **The cost is** that the runner cannot simply call
/// `ra_prompt::Assembler::new()`; the capability has to be expressed as a `ra-core` contract
/// first. That is precisely the intent — wherever this proves unavoidable, the contract belonged
/// in the kernel all along, rather than the kernel learning about one concrete implementation.
const ALLOWED_INTERNAL_DEPS: &[(&str, &[&str])] = &[
    ("ra-core", &[]),
    ("ra-macros", &[]),
    // See above: this is dependency inversion, not something nobody got around to adding.
    ("ra-runtime", &["ra-core"]),
    ("ra-model", &["ra-core"]),
    ("ra-prompt", &["ra-core"]),
    ("ra-context", &["ra-core"]),
    ("ra-exec", &["ra-core"]),
    ("ra-session", &["ra-core"]),
    ("ra-mcp", &["ra-core"]),
    ("ra-protocol", &["ra-core", "ra-session"]),
    (
        "ra-eval",
        &["ra-core", "ra-runtime", "ra-model", "ra-protocol"],
    ),
    // `ra-macros` is the schema derive (R2-2). Every crate that *declares* a tool needs it, and it
    // is a kernel crate with no dependencies of its own, so this widens nothing: the alternative is
    // hand-written JSON schemas that skip strict normalization and the typed decoder.
    //
    // `ra-patch` is the same shape as `ra-exec` and `ra-mcp` here: a service crate holding the work
    // that one tool entry point binds to. It arrived when `apply_patch` moved in, which is also
    // when the reusable-piece test stopped having an exception.
    (
        "ra-tools",
        &["ra-core", "ra-exec", "ra-mcp", "ra-macros", "ra-patch"],
    ),
    ("ra-flow", &["ra-core", "ra-runtime"]),
    // `ra-patch` parses and applies a text format and needs nothing from the framework to do it.
    // What it does need is `ra-core::compat`: a `PatchPlan` and a `CommittedPatchDelta` are records
    // that get persisted and read back by a build that may be older or newer, and a second
    // `SchemaVersion` / `Unknown` pair declared here would make "does this record need migrating"
    // answerable through `Compatibility` for a rollout line and not for a patch plan. The
    // dependency buys the compat vocabulary and nothing else — no runtime, no tool contract.
    ("ra-patch", &["ra-core"]),
    // `ra-macros` is gone from this list: it was here only for the `apply_patch` input schema, and
    // that tool now lives in `ra-tools`. `ra-patch` stays — the dangerous-action detector reads a
    // `PatchPlan` to produce approval facts, without applying anything.
    (
        "ra-coding",
        &[
            "ra-core",
            "ra-runtime",
            "ra-prompt",
            "ra-context",
            "ra-exec",
            "ra-mcp",
            "ra-patch",
            "ra-tools",
        ],
    ),
    (
        "ra-assistant",
        &["ra-core", "ra-runtime", "ra-tools", "ra-flow", "ra-prompt"],
    ),
    (
        "ra-cli",
        &["ra-coding", "ra-assistant", "ra-protocol", "ra-eval"],
    ),
    ("xtask", &[]),
];

fn layer_of(name: &str) -> Option<Layer> {
    LAYERS
        .iter()
        .find(|(crate_name, _)| *crate_name == name)
        .map(|(_, layer)| *layer)
}

/// Runs the gate.
pub(crate) fn run() -> Outcome {
    let graph = match workspace_graph() {
        Ok(graph) => graph,
        Err(err) => return Outcome::Fail(vec![format!("读取 cargo metadata 失败：{err}")]),
    };

    let mut violations = Vec::new();

    // An unregistered crate is stopped first: without a layer there is no way to judge its
    // dependencies.
    for name in graph.keys() {
        if layer_of(name).is_none() {
            violations.push(format!(
                "crate `{name}` 未登记层级——请在 xtask/src/layering.rs 的 LAYERS 里决定它属于哪一层"
            ));
        }
    }
    if !violations.is_empty() {
        return Outcome::Fail(violations);
    }

    violations.extend(check_declared_boundaries(&graph));
    violations.extend(check_dependencies(&graph));
    let (product_violations, exemptions) = check_product_references(&graph);
    violations.extend(product_violations);
    violations.extend(check_module_layout());

    let exempted = if exemptions == 0 {
        String::new()
    } else {
        format!("（{exemptions} 处 `{ALLOW_MARKER}` 显式例外）")
    };
    Outcome::from_violations(
        violations,
        format!(
            "{} 个 crate，依赖边界与现代模块布局全部满足{exempted}",
            graph.len()
        ),
    )
}

/// Since Rust 2018 a submodule no longer needs `mod.rs`. Using `foo.rs` plus `foo/` throughout
/// keeps the entry visible at a glance in a file listing and avoids two layouts coexisting
/// indefinitely.
fn check_module_layout() -> Vec<String> {
    let crates = source::workspace_root().join("crates");
    source::rust_files(&crates)
        .into_iter()
        .filter(|file| file.file_name().is_some_and(|name| name == "mod.rs"))
        .map(|file| {
            format!(
                "{}：禁止旧式 `mod.rs`，请迁移为同级 `foo.rs + foo/`",
                source::relative(&file),
            )
        })
        .collect()
}

/// Checks the direct-dependency boundaries declared entry by entry in the crate table.
fn check_declared_boundaries(graph: &Graph) -> Vec<String> {
    let allowed: BTreeMap<&str, BTreeSet<&str>> = ALLOWED_INTERNAL_DEPS
        .iter()
        .map(|(name, deps)| (*name, deps.iter().copied().collect()))
        .collect();
    let mut violations = Vec::new();

    for (name, deps) in graph {
        let Some(expected) = allowed.get(name.as_str()) else {
            violations.push(format!(
                "crate `{name}` 缺直接依赖白名单——请在 ALLOWED_INTERNAL_DEPS 登记"
            ));
            continue;
        };
        for dep in deps {
            if !expected.contains(dep.name.as_str()) {
                violations.push(format!(
                    "{name} 直接依赖了职责表未允许的 {} [{}]——若这是新契约，先更新 crate 边界文档与白名单",
                    dep.name, dep.kind,
                ));
            }
        }
    }

    violations
}

// ---------------------------------------------------------------------------
// dependency graph
// ---------------------------------------------------------------------------

/// One internal dependency.
struct Dep {
    name: String,
    /// `normal` / `dev` / `build`. Dev dependencies are bound too: bypassing layering in a test
    /// is still bypassing it.
    kind: String,
}

/// The internal dependency graph: crate name -> the internal crates it depends on directly.
type Graph = BTreeMap<String, Vec<Dep>>;

fn workspace_graph() -> Result<Graph, String> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let output = Command::new(cargo)
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .current_dir(source::workspace_root())
        .output()
        .map_err(|e| e.to_string())?;

    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
    }

    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).map_err(|e| e.to_string())?;
    let packages = json
        .get("packages")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "metadata 里没有 packages 数组".to_owned())?;

    let names: BTreeSet<String> = packages
        .iter()
        .filter_map(|p| p.get("name").and_then(serde_json::Value::as_str))
        .map(ToOwned::to_owned)
        .collect();

    let mut graph = Graph::new();
    for package in packages {
        let Some(name) = package.get("name").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let deps = package
            .get("dependencies")
            .and_then(serde_json::Value::as_array)
            .map(|deps| {
                deps.iter()
                    .filter_map(|d| {
                        let dep_name = d.get("name").and_then(serde_json::Value::as_str)?;
                        // Only internal crates matter; third-party dependencies are outside
                        // layering.
                        if !names.contains(dep_name) {
                            return None;
                        }
                        Some(Dep {
                            name: dep_name.to_owned(),
                            kind: d
                                .get("kind")
                                .and_then(serde_json::Value::as_str)
                                .unwrap_or("normal")
                                .to_owned(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        graph.insert(name.to_owned(), deps);
    }

    Ok(graph)
}

/// Every internal crate reachable from `start`, each with a path that reaches it.
fn reachable(graph: &Graph, start: &str) -> BTreeMap<String, Vec<String>> {
    let mut seen: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut queue = VecDeque::new();
    queue.push_back((start.to_owned(), vec![start.to_owned()]));

    while let Some((current, path)) = queue.pop_front() {
        let Some(deps) = graph.get(&current) else {
            continue;
        };
        for dep in deps {
            if seen.contains_key(&dep.name) || dep.name == start {
                continue;
            }
            let mut next = path.clone();
            next.push(dep.name.clone());
            seen.insert(dep.name.clone(), next.clone());
            queue.push_back((dep.name.clone(), next));
        }
    }
    seen
}

/// The dependency side of rules 1 through 3.
///
/// **Direct violations are listed before transitive ones**: one wrong `Cargo.toml` dependency
/// explodes into a dozen transitive violations downstream and buries the single line that needs
/// editing. Root cause first, so reading the first entry is enough to fix it.
fn check_dependencies(graph: &Graph) -> Vec<String> {
    let mut direct = Vec::new();
    let mut transitive = Vec::new();

    for (name, deps) in graph {
        let Some(layer) = layer_of(name) else {
            continue;
        };

        // Direct dependency: the kind is available, which makes the message more useful.
        for dep in deps {
            let Some(dep_layer) = layer_of(&dep.name) else {
                continue;
            };
            if let Some(rule) = forbidden(layer, dep_layer) {
                direct.push(format!(
                    "{}（{}）依赖了 {}（{}）[{}]：{rule}",
                    name,
                    layer.label(),
                    dep.name,
                    dep_layer.label(),
                    dep.kind,
                ));
            }
        }

        // Transitive dependency: checking only direct edges would miss ra-core -> X -> ra-coding.
        for (target, path) in reachable(graph, name) {
            let Some(target_layer) = layer_of(&target) else {
                continue;
            };
            let is_direct = deps.iter().any(|d| d.name == target);
            if is_direct {
                continue; // 上面已报过
            }
            if let Some(rule) = forbidden(layer, target_layer) {
                transitive.push(format!(
                    "{}（{}）经 {} 传递依赖了 {}（{}）：{rule}",
                    name,
                    layer.label(),
                    path.join(" -> "),
                    target,
                    target_layer.label(),
                ));
            }
        }
    }

    direct.extend(transitive);
    direct
}

/// Whether `from` depending on `to` is a violation; if so, returns the rule it breaks.
///
/// Only the explicitly forbidden combinations are encoded; **no total order is invented**.
/// `ra-eval` (a service) depending on `ra-runtime` (kernel) and `ra-coding` (a product) depending
/// on `ra-runtime` (kernel) are both legal, which shows the layers were never comparable in one
/// direction to begin with.
const fn forbidden(from: Layer, to: Layer) -> Option<&'static str> {
    match (from, to) {
        (Layer::Kernel, Layer::Reusable | Layer::Product) => {
            Some("铁律 1：内核不依赖可复用件与产品")
        }
        (Layer::Reusable, Layer::Product) => Some("铁律 2：可复用件不依赖产品"),
        (Layer::Product, Layer::Product) => Some("铁律 3：产品之间零依赖"),
        (Layer::Service, Layer::Product) => Some("铁律 3 推论：框架服务层不依赖产品"),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// source side: the `use` of rule 3 and the product-name branching of rule 4
// ---------------------------------------------------------------------------

/// A product name appearing in a framework crate, whether as a `use` or as a string branch.
///
/// Returns the violations plus the number of explicit exemptions allowed through. That count goes
/// into the summary output: it can only grow one entry at a time, and every CI run shows whether
/// it did.
fn check_product_references(graph: &Graph) -> (Vec<String>, usize) {
    let root = source::workspace_root();
    let products: Vec<_> = LAYERS
        .iter()
        .filter_map(|(name, layer)| (*layer == Layer::Product).then_some(*name))
        .collect();
    let mut violations = Vec::new();
    let mut exemptions = 0_usize;

    for name in graph.keys() {
        let Some(layer) = layer_of(name) else {
            continue;
        };
        if !layer.is_framework() {
            continue; // 产品自己和 ra-cli 允许提产品名
        }

        let src = root.join("crates").join(name).join("src");
        for file in source::rust_files(&src) {
            let Ok(text) = std::fs::read_to_string(&file) else {
                continue;
            };
            let scan = scan_product_references(&text, &products);
            exemptions += scan.exemptions();
            for violation in scan.violations() {
                violations.push(format!(
                    "{}:{}：框架 crate `{}`（{}）违反产品隔离：{}。\
                     协议词汇确实撞名时使用 `// {ALLOW_MARKER} <alias> = <理由>`",
                    source::relative(&file),
                    violation.line(),
                    name,
                    layer.label(),
                    violation.message(),
                ));
            }
        }
    }

    (violations, exemptions)
}
