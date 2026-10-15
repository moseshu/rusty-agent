//! Test host dependency checks must distinguish Cargo package identity from an import alias.

use xtask::tests_workspace_policy::has_path_dependency;

#[test]
fn ordinary_and_renamed_dependencies_identify_the_same_package() {
    for manifest in [
        "[dev-dependencies]\nrusty-agent = { path = \"../../crates/rusty-agent\" }",
        "[dev-dependencies]\nagents = { package = \"rusty-agent\", path = \"../../crates/rusty-agent\" }",
        "[dependencies.agents]\npath = \"../../crates/rusty-agent\"\npackage = \"rusty-agent\"",
    ] {
        assert!(has_path_dependency(manifest, "rusty-agent"));
    }
}

#[test]
fn comments_other_packages_other_checkouts_and_invalid_toml_do_not_satisfy_a_host() {
    for manifest in [
        "# rusty-agent = { path = \"../../crates/rusty-agent\" }",
        "[dev-dependencies]\nrusty-agent = { path = \"somewhere-else\" }",
        "[dev-dependencies]\nrusty-agent = { package = \"other-package\", path = \"../../crates/rusty-agent\" }",
        "[dev-dependencies]\nrusty-agent = \"0.2.2\"",
        "[dev-dependencies\n",
    ] {
        assert!(!has_path_dependency(manifest, "rusty-agent"));
    }
}
