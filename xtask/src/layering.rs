//! `layering` 门禁：依赖方向四条铁律。
//!
//! 铁律出自[开发计划](../../Docs/Rusty_Agent_Framework_Development_Plan.md)的 crate
//! 表与[项目结构](../../Docs/Rusty_Agent_Project_Structure.md) §1：
//!
//! 1. **内核**（`ra-core` / `ra-macros` / `ra-runtime`）不依赖可复用件与产品；
//! 2. **可复用件**不依赖产品；
//! 3. **产品之间零依赖**，且框架 crate 不得 `use ra_coding` / `use ra_assistant`；
//! 4. **框架 crate 里零按产品名分支**（R18-8）。
//!
//! 前三条查依赖图（**含传递依赖**——`ra-core → X → ra-coding` 同样是违规，只查直接
//! 依赖会漏），第四条查源码。
//!
//! 层级表是**白名单**：新建一个没登记的 crate 会让门禁直接失败。这是刻意的——
//! 新 crate 落在哪一层是必须当场做的决定，不是可以以后再说的事。

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::process::Command;

use crate::gate::Outcome;
use crate::source;

/// crate 所属的层。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Layer {
    /// 内核：零业务的框架机制。
    Kernel,
    /// 通用服务：provider、沙箱、会话等。
    Service,
    /// 可复用件：换业务不改，但每个产品都要用。
    Reusable,
    /// 参考产品：业务内容。
    Product,
    /// 二进制入口。装配者，允许依赖产品。
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

    /// 是否属于框架（与「产品内容」和「装配者」相对）。
    const fn is_framework(self) -> bool {
        matches!(self, Self::Kernel | Self::Service | Self::Reusable)
    }
}

/// 层级白名单。目标形态 17 个 crate，尚未建的（`ra-tools` / `ra-flow` /
/// `ra-assistant`）先登记着，建出来当天就受门禁约束。
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

fn layer_of(name: &str) -> Option<Layer> {
    LAYERS
        .iter()
        .find(|(crate_name, _)| *crate_name == name)
        .map(|(_, layer)| *layer)
}

/// 产品 crate 的名字，用于第 3、4 条的源码扫描。
const PRODUCTS: &[&str] = &["ra-coding", "ra-assistant"];

/// 执行门禁。
pub(crate) fn run() -> Outcome {
    let graph = match workspace_graph() {
        Ok(graph) => graph,
        Err(err) => return Outcome::Fail(vec![format!("读取 cargo metadata 失败：{err}")]),
    };

    let mut violations = Vec::new();

    // 未登记的 crate 先拦下来：没有层级就无从判断它的依赖是否合法。
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

    violations.extend(check_dependencies(&graph));
    violations.extend(check_product_references(&graph));

    Outcome::from_violations(
        violations,
        format!("{} 个 crate，四条铁律全部满足", graph.len()),
    )
}

// ---------------------------------------------------------------------------
// 依赖图
// ---------------------------------------------------------------------------

/// 一条内部依赖。
struct Dep {
    name: String,
    /// `normal` / `dev` / `build`。dev 依赖同样受约束——测试里绕过分层也是绕过。
    kind: String,
}

/// 内部依赖图：crate 名 → 它直接依赖的内部 crate。
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
                        // 只关心内部 crate；第三方依赖不参与分层。
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

/// 从 `start` 出发能到达的全部内部 crate，附一条抵达路径。
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

/// 铁律 1-3 的依赖侧。
///
/// **直接违规排在传递违规前面**：一条错误的 `Cargo.toml` 依赖会在下游炸出十几条
/// 传递违规，把唯一需要动手的那一行埋在中间。根因先行，读第一条就能修。
fn check_dependencies(graph: &Graph) -> Vec<String> {
    let mut direct = Vec::new();
    let mut transitive = Vec::new();

    for (name, deps) in graph {
        let Some(layer) = layer_of(name) else {
            continue;
        };

        // 直接依赖：能给出 kind，报错信息更有用。
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

        // 传递依赖：只查直接依赖会漏掉 ra-core -> X -> ra-coding。
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

/// `from` 依赖 `to` 是否违规；违规则返回被违反的铁律。
///
/// 只编码被明确禁止的组合，**不发明一个全序**：`ra-eval`（服务）依赖 `ra-runtime`
/// （内核）与 `ra-coding`（产品）依赖 `ra-runtime`（内核）都是合法的，说明层级之间
/// 本来就不是单向可比的。
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
// 源码侧：铁律 3 的 `use` 与铁律 4 的产品名分支
// ---------------------------------------------------------------------------

/// 框架 crate 里出现产品名——不管是 `use` 还是字符串分支。
fn check_product_references(graph: &Graph) -> Vec<String> {
    let root = source::workspace_root();
    let mut violations = Vec::new();

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
            for (index, line) in text.lines().enumerate() {
                if source::is_comment(line) {
                    continue;
                }
                for product in PRODUCTS {
                    let snake = product.replace('-', "_");
                    // `use ra_coding::…` / `ra_coding::Foo`（铁律 3）
                    // `"coding"` / `"assistant"` 这类按产品名分支（铁律 4）
                    let quoted = format!("\"{}\"", product.trim_start_matches("ra-"));
                    if line.contains(&snake) || line.contains(&quoted) {
                        violations.push(format!(
                            "{}:{}：框架 crate `{}`（{}）里出现产品名 `{}` —— \
                             铁律 3/4：差异只能由 profile / capability / prompt / guard 注册表达",
                            source::relative(&file),
                            index + 1,
                            name,
                            layer.label(),
                            product,
                        ));
                    }
                }
            }
        }
    }

    violations
}
