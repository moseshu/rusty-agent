//! Structural contract of the separate test workspace (R0-7).
//!
//! The host list is not hard-coded: it is derived from `crates/` by the same rule `cargo xtask
//! test` uses. A hard-coded table only ever gets forgotten on the day a crate is created, which is
//! exactly when the contract should speak.

use std::path::{Path, PathBuf};

/// The host for cross-crate contracts; it corresponds to no single crate under test.
const CROSS_CRATE_HOST: &str = "it-e2e";

#[test]
fn 每个库_crate_都有独立宿主和路径依赖() {
    let root = repository_root();
    let tests_manifest = read(&root.join("tests/Cargo.toml"));
    let crates = library_crates(&root);

    assert!(
        crates.len() >= 13,
        "只发现 {} 个库 crate，推导规则可能坏了",
        crates.len()
    );

    for crate_name in crates {
        let host = host_of(&crate_name);
        assert!(
            tests_manifest.contains(&format!("\"{host}\"")),
            "tests/Cargo.toml 缺宿主 `{host}`（`crates/{crate_name}` 是库 crate）"
        );

        let host_manifest = root.join("tests").join(&host).join("Cargo.toml");
        let text = read(&host_manifest);
        let expected_path = format!("{crate_name} = {{ path = \"../../crates/{crate_name}\"");
        assert!(
            text.contains(&expected_path),
            "{} 没有通过 path 依赖被测 crate `{crate_name}`",
            host_manifest.display()
        );
    }

    assert!(
        tests_manifest.contains(&format!("\"{CROSS_CRATE_HOST}\"")),
        "tests/Cargo.toml 缺跨 crate 契约宿主 `{CROSS_CRATE_HOST}`"
    );
}

#[test]
fn 测试_workspace_独立于主_workspace_且断言进版本库() {
    let root = repository_root();
    let main_manifest = read(&root.join("Cargo.toml"));
    let ignore = read(&root.join(".gitignore"));
    let ignored: Vec<&str> = ignore.lines().map(str::trim).collect();

    assert!(
        !main_manifest.contains("tests/it-"),
        "测试宿主不能进主 workspace 的 members：测试专用依赖会污染主依赖图"
    );
    assert!(
        !ignored.contains(&"/tests/"),
        "整目录忽略 /tests/ 会让 115 条断言退回单机本地，CI 只能报 SKIP"
    );
    assert!(
        ignored.contains(&"/tests/target/"),
        ".gitignore 必须忽略 /tests/target/：构建产物不进版本库"
    );
}

#[test]
fn crates_里没有测试代码和旧式_mod_rs() {
    let crates = repository_root().join("crates");
    for file in rust_files(&crates) {
        assert_ne!(
            file.file_name().and_then(|name| name.to_str()),
            Some("mod.rs"),
            "旧式模块入口残留：{}",
            file.display()
        );

        let text = read(&file);
        assert!(
            !text.contains("cfg(test")
                && !text.contains("#[test]")
                && !text.contains("#[tokio::test]"),
            "测试代码必须移到 tests/：{}",
            file.display()
        );
    }

    for entry in std::fs::read_dir(&crates).expect("crates/ should be readable") {
        let path = entry.expect("crate entry should be readable").path();
        assert!(
            !path.join("tests").is_dir(),
            "Cargo 集成测试位必须移到独立 workspace：{}",
            path.display()
        );
    }
}

#[test]
fn 每个测试源文件都至少包含一个真实测试() {
    let root = repository_root();
    let tests = root.join("tests");
    let mut sources = 0_usize;

    for host in library_crates(&root)
        .iter()
        .map(|name| host_of(name))
        .chain(std::iter::once(CROSS_CRATE_HOST.to_owned()))
    {
        for file in rust_files(&tests.join(host).join("tests")) {
            sources += 1;
            let text = read(&file);
            assert!(
                has_test_attribute(&text),
                "空占位测试会制造假绿：{}",
                file.display()
            );
        }
    }

    assert!(sources > 0, "独立测试 workspace 至少要有一个行为测试");
}

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repository root should exist")
}

/// The crates under `crates/` that have a `src/lib.rs`. `ra-cli` is a binary and is not among
/// them.
fn library_crates(root: &Path) -> Vec<String> {
    let crates = root.join("crates");
    let mut names = Vec::new();
    for entry in std::fs::read_dir(&crates).expect("crates/ should be readable") {
        let path = entry.expect("crate entry should be readable").path();
        if path.join("src").join("lib.rs").is_file()
            && let Some(name) = path.file_name().and_then(|name| name.to_str())
        {
            names.push(name.to_owned());
        }
    }
    names.sort();
    names
}

/// `ra-core` → `it-core`。
fn host_of(crate_name: &str) -> String {
    format!("it-{}", crate_name.trim_start_matches("ra-"))
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()))
}

/// Match the attribute name rather than the whole line: `#[tokio::test(flavor = "multi_thread")]`
/// and a parameterized `#[rstest(..)]` are real tests too, and a whole-line comparison would
/// misjudge them as empty placeholders.
fn has_test_attribute(text: &str) -> bool {
    const MARKERS: &[&str] = &["test", "tokio::test", "rstest", "test_case"];
    text.lines().any(|line| {
        let Some(rest) = line.trim().strip_prefix("#[") else {
            return false;
        };
        MARKERS.iter().any(|marker| {
            rest.strip_prefix(marker)
                .is_some_and(|tail| tail.starts_with(']') || tail.starts_with('('))
        })
    })
}

fn rust_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    collect_rust_files(root, &mut files);
    files.sort();
    files
}

fn collect_rust_files(root: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|name| name == "target") {
                continue;
            }
            collect_rust_files(&path, files);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            files.push(path);
        }
    }
}
