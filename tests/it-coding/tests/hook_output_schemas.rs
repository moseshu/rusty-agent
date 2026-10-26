//! Compare output wire schemas with fixed, independently copied upstream fixtures.
#![allow(clippy::expect_used)]

#[test]
fn command_hook_output_schemas_match_all_upstream_fixtures() {
    let schemas = ra_coding::hooks::test_api::output_schemas().expect("output schemas");
    assert_eq!(schemas.len(), 11);
    for (name, actual) in schemas {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/hooks_output")
            .join(name);
        let expected: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).expect("upstream fixture"))
                .expect("schema JSON");
        assert_eq!(actual, expected, "schema {name}");
    }
}
