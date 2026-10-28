//! Regression coverage for the exact scope of upstream API exceptions.

use xtask::extension_safety_policy::check_source;

const CONFIG: &str = "crates/ra-protocol/src/server/protocol/config.rs";

#[test]
fn upstream_exception_requires_both_the_file_and_type() {
    let source = "pub struct ConfigReadParams { pub cwd: Option<String> }";
    assert!(check_source(CONFIG, source).unwrap().is_empty());
    for path in [
        "crates/ra-coding/src/config.rs",
        "crates/ra-protocol/src/server/protocol/config.rs.backup",
        "crates/ra-protocol/src/server/protocol/config.rs/other.rs",
    ] {
        assert_eq!(check_source(path, source).unwrap().len(), 1, "{path}");
    }
    let source = "pub struct NewRecord { pub cwd: Option<String> }";
    assert_eq!(check_source(CONFIG, source).unwrap().len(), 1);
}

#[test]
fn enum_exception_does_not_escape_to_a_same_named_type_elsewhere() {
    let source = "pub enum NextStep { Continue, Stop }";
    assert!(
        check_source("crates/ra-core/src/step.rs", source)
            .unwrap()
            .is_empty()
    );
    assert_eq!(check_source(CONFIG, source).unwrap().len(), 1);
    assert_eq!(
        check_source("crates/ra-core/src/step.rs.extra", source)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn wire_enum_exception_does_not_exempt_custom_wrappers() {
    let source = "pub enum MergeStrategy { Replace, Upsert }";
    assert!(check_source(CONFIG, source).unwrap().is_empty());
    assert_eq!(
        check_source("crates/ra-protocol/src/server.rs", source)
            .unwrap()
            .len(),
        1
    );
    let source = "pub enum PluginManagementRequest { Read }";
    assert_eq!(check_source(CONFIG, source).unwrap().len(), 1);
}

#[test]
fn ordinary_encapsulated_and_non_exhaustive_types_pass() {
    let source = r"
        pub struct State { generation: u64 }
        #[non_exhaustive]
        pub enum Outcome { Ready, Failed }
        mod nested {
            pub struct NewRecord { pub value: bool }
            pub enum NewOutcome { Ready }
        }
    ";
    assert_eq!(check_source(CONFIG, source).unwrap().len(), 2);
}

#[test]
fn windows_paths_use_the_same_exact_exceptions() {
    let source = "pub enum MergeStrategy { Replace, Upsert }";
    assert!(
        check_source(&CONFIG.replace('/', "\\"), source)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn invalid_source_is_an_error() {
    assert!(check_source(CONFIG, "pub enum Broken {").is_err());
}

#[test]
fn inventoried_protocol_files_pass_without_module_wide_exemptions() {
    for (path, source) in [
        (
            CONFIG,
            include_str!("../../../crates/ra-protocol/src/server/protocol/config.rs"),
        ),
        (
            "crates/ra-protocol/src/server/protocol/plugin_management.rs",
            include_str!("../../../crates/ra-protocol/src/server/protocol/plugin_management.rs"),
        ),
        (
            "crates/ra-protocol/src/server/protocol/hooks.rs",
            include_str!("../../../crates/ra-protocol/src/server/protocol/hooks.rs"),
        ),
    ] {
        assert!(check_source(path, source).unwrap().is_empty(), "{path}");
        assert_eq!(
            check_source(path, "pub struct ExtraRecord { pub value: bool }")
                .unwrap()
                .len(),
            1
        );
    }
}

#[test]
fn native_process_and_mcp_source_exceptions_are_exactly_scoped() {
    for (path, source) in [
        (
            "crates/ra-exec/src/native_process/child.rs",
            include_str!("../../../crates/ra-exec/src/native_process/child.rs"),
        ),
        (
            "crates/ra-exec/src/native_process/child_command.rs",
            include_str!("../../../crates/ra-exec/src/native_process/child_command.rs"),
        ),
        (
            "crates/ra-mcp/src/lib.rs",
            include_str!("../../../crates/ra-mcp/src/lib.rs"),
        ),
        (
            "crates/ra-mcp/src/registration_catalog.rs",
            include_str!("../../../crates/ra-mcp/src/registration_catalog.rs"),
        ),
    ] {
        assert!(check_source(path, source).unwrap().is_empty(), "{path}");
        assert!(
            !check_source(&format!("{path}.other"), source)
                .unwrap()
                .is_empty(),
            "{path}"
        );
        assert_eq!(
            check_source(path, "pub struct UnportedWrapper { pub value: bool }")
                .unwrap()
                .len(),
            1
        );
    }
}
