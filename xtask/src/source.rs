//! 源码遍历的公共 helper。

use std::path::{Path, PathBuf};

/// workspace 根目录。
///
/// 从编译期的 `CARGO_MANIFEST_DIR` 推，不看当前工作目录——门禁在哪儿调用都该查
/// 同一份代码。
pub(crate) fn workspace_root() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest.parent().unwrap_or(manifest).to_path_buf()
}

/// 相对 workspace 根的路径，用于报错信息。
pub(crate) fn relative(path: &Path) -> String {
    path.strip_prefix(workspace_root())
        .unwrap_or(path)
        .display()
        .to_string()
}

/// 递归收集目录下的 `.rs` 文件。目录不存在时返回空。
pub(crate) fn rust_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    collect(dir, &mut files);
    files.sort();
    files
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

/// 整行是注释。
///
/// 只认行首注释，**不做词法分析**：门禁宁可漏报也不该误报——把一条合法代码判成
/// 违规会让人开始怀疑门禁本身，那比漏一条更糟。行尾注释里的关键字会被漏掉，这是
/// 已知且可接受的代价。
pub(crate) fn is_comment(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("//") || trimmed.starts_with("/*") || trimmed.starts_with('*')
}
