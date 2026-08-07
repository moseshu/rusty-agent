//! `test` 门禁：跑独立测试 workspace。
//!
//! 测试不在主 workspace 里（见[项目结构](../../Docs/Rusty_Agent_Project_Structure.md)
//! §5.5），`cargo test --workspace` 跑不到任何行为断言。这条门禁就是那个入口。
//!
//! `tests/` **进版本库**，因此缺少它不是"本机没有"而是 checkout 坏了——直接 FAIL，
//! 不跳过。

use std::path::Path;
use std::process::Command;

use crate::gate::Outcome;
use crate::source;

/// 跨 crate 契约的宿主，不对应任何单个被测 crate。
const CROSS_CRATE_HOST: &str = "it-e2e";

/// 执行门禁。`args` 透传给 `cargo test`（如 `-p it-core`）。
pub(crate) fn run(args: &[String]) -> Outcome {
    let manifest = source::workspace_root().join("tests").join("Cargo.toml");
    if !manifest.is_file() {
        return Outcome::Fail(vec![format!(
            "{} 不存在——tests/ 是版本库的一部分，缺失说明 checkout 不完整",
            source::relative(&manifest),
        )]);
    }

    let violations = validate_layout(&manifest);
    if !violations.is_empty() {
        return Outcome::Fail(violations);
    }

    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let status = Command::new(cargo)
        .arg("test")
        .arg("--manifest-path")
        .arg(&manifest)
        .args(args)
        .current_dir(source::workspace_root())
        .status();

    match status {
        Ok(status) if status.success() => Outcome::pass("测试 workspace 全绿"),
        Ok(status) => Outcome::Fail(vec![format!("cargo test 退出码 {status}")]),
        Err(err) => Outcome::Fail(vec![format!("无法启动 cargo test：{err}")]),
    }
}

/// 被测 crate → 它的测试宿主名。`ra-core` → `it-core`。
fn host_of(crate_name: &str) -> String {
    format!("it-{}", crate_name.trim_start_matches("ra-"))
}

/// 在启动 Cargo 前先拒绝两类“假绿”：漏宿主，以及只有注释、没有断言的测试文件。
///
/// 宿主清单**从 `crates/` 推导**而不是写死：写死的表只会在新建 crate 那天忘记更新，
/// 而那正是门禁最该说话的时刻。
fn validate_layout(manifest: &Path) -> Vec<String> {
    let tests_root = manifest.parent().unwrap_or(manifest);
    let workspace = std::fs::read_to_string(manifest).unwrap_or_default();
    let mut violations = Vec::new();
    let mut hosts = Vec::new();

    for crate_name in source::library_crates() {
        let host = host_of(&crate_name);
        if !workspace.contains(&format!("\"{host}\"")) {
            violations.push(format!(
                "tests/Cargo.toml 缺测试宿主 `{host}`——`crates/{crate_name}` 是库 crate，必须有一个"
            ));
            continue;
        }
        hosts.push(host.clone());

        let host_manifest = tests_root.join(&host).join("Cargo.toml");
        let text = std::fs::read_to_string(&host_manifest).unwrap_or_default();
        let expected = format!("{crate_name} = {{ path = \"../../crates/{crate_name}\"");
        if !text.contains(&expected) {
            violations.push(format!(
                "{}：宿主 `{host}` 没有通过 path 依赖 `{crate_name}`",
                source::relative(&host_manifest),
            ));
        }
    }

    if workspace.contains(&format!("\"{CROSS_CRATE_HOST}\"")) {
        hosts.push(CROSS_CRATE_HOST.to_owned());
    } else {
        violations.push(format!(
            "tests/Cargo.toml 缺跨 crate 契约宿主 `{CROSS_CRATE_HOST}`"
        ));
    }

    let mut test_files = 0_usize;
    let mut test_cases = 0_usize;
    for host in &hosts {
        // 只看 Cargo 的外部测试位：`src/` 是空 lib，`fixtures/` 是编译期负向夹具，
        // 两者都不该被当成"缺断言的测试文件"。
        for file in source::rust_files(&tests_root.join(host).join("tests")) {
            test_files += 1;
            let text = std::fs::read_to_string(&file).unwrap_or_default();
            let count = text.lines().filter(|line| is_test_attribute(line)).count();
            if count == 0 {
                violations.push(format!(
                    "{}：空占位测试没有任何测试函数——实现行为时再创建，不能用零测试制造假绿",
                    source::relative(&file),
                ));
            }
            test_cases += count;
        }
    }

    if test_files == 0 || test_cases == 0 {
        violations.push("独立测试 workspace 没有任何有效行为断言".to_owned());
    }

    violations
}

/// 一行是否是测试标记属性。
///
/// **匹配属性名，不比较整行**：`#[tokio::test(flavor = "multi_thread")]` 与
/// `#[rstest(case(1), case(2))]` 都是真测试，按整行精确比较会把只写参数化测试的
/// 文件判成空占位——门禁误报比漏报更糟（理由同 [`source::is_comment`]）。
fn is_test_attribute(line: &str) -> bool {
    const MARKERS: &[&str] = &["test", "tokio::test", "rstest", "test_case"];
    let Some(rest) = line.trim().strip_prefix("#[") else {
        return false;
    };
    MARKERS.iter().any(|marker| {
        rest.strip_prefix(marker)
            // `]` 是无参属性，`(` 是带参属性。靠这一步把 `#[test]` 跟 `#[test_case(..)]`
            // 和 `#[should_panic]` 区分开——前缀匹配不加这层会全都算成测试。
            .is_some_and(|tail| tail.starts_with(']') || tail.starts_with('('))
    })
}
