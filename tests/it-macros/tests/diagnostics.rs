//! Expansion diagnostics must fail compilation at the offending declaration.

#[test]
fn unsupported_declarations_have_actionable_diagnostics() {
    let cases = trybuild::TestCases::new();
    cases.compile_fail("tests/ui/*.rs");
}
