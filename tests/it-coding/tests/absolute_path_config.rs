#![allow(clippy::expect_used)]

use ra_execpolicy::AbsolutePathBuf;
use tempfile::tempdir;

#[test]
fn permission_guard_and_config_paths_share_one_scope() {
    let base = tempdir().expect("base");
    let home = tempdir().expect("home");
    ra_coding::permissions::AbsolutePathBufGuard::with_home_directory(home.path(), || {
        let _guard = ra_coding::permissions::AbsolutePathBufGuard::new(base.path());
        let path: AbsolutePathBuf = serde_json::from_str("\"~/plugin\"").expect("path");
        assert_eq!(path.as_path(), home.path().join("plugin"));
        let path: AbsolutePathBuf = serde_json::from_str("\"config.toml\"").expect("path");
        assert_eq!(path.as_path(), base.path().join("config.toml"));
    });
    assert!(serde_json::from_str::<AbsolutePathBuf>("\"~/plugin\"").is_err());
}

#[test]
fn permission_adapters_reject_nul_paths_after_shared_normalization() {
    use ra_coding::permissions::FileSystemPath;

    let base = tempdir().expect("base");
    let invalid = base.path().join("bad\0name");
    assert!(ra_paths::AbsolutePathBuf::from_absolute_path(&invalid).is_ok());
    assert!(FileSystemPath::path(invalid).is_err());
    let _guard = ra_paths::AbsolutePathBufGuard::new(base.path());
    let value = serde_json::json!({ "type": "path", "path": "bad\0name" });
    let error = serde_json::from_value::<FileSystemPath>(value).expect_err("NUL path");
    assert!(
        error
            .to_string()
            .contains("permission path cannot be resolved")
    );
}
