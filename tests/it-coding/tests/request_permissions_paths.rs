//! Codex's legacy API path resolution using an explicitly supplied executor context.

use std::path::Path;

use ra_coding::{
    permissions::FileSystemSandboxPolicyContext, request_permissions_test_api::resolve_path,
};
use serde_json::json;

#[test]
fn home_relative_paths_require_an_executor_home() {
    let context = FileSystemSandboxPolicyContext::new(Path::new("/workspace"), &[]);
    for raw in ["~", "~/secret"] {
        let mut value = json!(raw);
        assert_eq!(
            resolve_path(&mut value, &context),
            Err(format!(
                "cannot resolve home-relative path `{raw}` without an executor home"
            ))
        );
        assert_eq!(value, json!(raw));
    }
}

#[test]
fn home_relative_paths_use_the_executor_home() {
    let context = FileSystemSandboxPolicyContext::new(Path::new("/workspace"), &[])
        .with_user_home_dir(Path::new("/executor-home"));
    for (raw, expected) in [
        ("~", "/executor-home"),
        ("~//secret/../out", "/executor-home/out"),
        ("~other", "/workspace/~other"),
    ] {
        let mut value = json!(raw);
        resolve_path(&mut value, &context).unwrap();
        assert_eq!(value, json!(expected));
    }
}
