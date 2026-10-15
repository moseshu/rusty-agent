//! Dependency identity checks shared by the test-workspace gate and its consumer tests.

/// Whether a test host depends on the requested package through its expected relative path.
///
/// Checks actual TOML dependency entries, including renamed packages; comments, malformed
/// manifests, a different package, or a different checkout do not satisfy the host requirement.
#[must_use]
pub fn has_path_dependency(manifest: &str, crate_name: &str) -> bool {
    let Ok(manifest) = manifest.parse::<toml::Value>() else {
        return false;
    };
    let expected_path = format!("../../crates/{crate_name}");
    ["dependencies", "dev-dependencies"].iter().any(|section| {
        manifest
            .get(*section)
            .and_then(toml::Value::as_table)
            .is_some_and(|dependencies| {
                dependencies.iter().any(|(name, dependency)| {
                    dependency
                        .get("package")
                        .and_then(toml::Value::as_str)
                        .unwrap_or(name)
                        == crate_name
                        && dependency.get("path").and_then(toml::Value::as_str)
                            == Some(expected_path.as_str())
                })
            })
    })
}
