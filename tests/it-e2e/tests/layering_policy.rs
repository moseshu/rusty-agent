//! Regression tests for product-name scanning and explicit exemptions.

use xtask::layering_policy::scan_product_references;

const PRODUCTS: &[&str] = &["ra-coding", "ra-assistant"];

fn messages(source: &str) -> Vec<String> {
    scan_product_references(source, PRODUCTS)
        .violations()
        .iter()
        .map(|violation| violation.message().to_owned())
        .collect()
}

#[test]
fn six_ordinary_product_branch_forms_are_rejected() {
    let cases = [
        r#"if profile == "assistant" {}"#,
        r#"match profile { "assistant" => run(), _ => {} }"#,
        r#"const PRODUCT: &str = "assistant";"#,
        r#"let product = "assistant";"#,
        r#"let selected = products.get("assistant");"#,
        r#"const PRODUCTS: &[&str] = &["coding", "assistant"];"#,
    ];

    for source in cases {
        let report = scan_product_references(source, PRODUCTS);
        assert!(
            !report.violations().is_empty(),
            "product branch escaped the scanner: {source}"
        );
        assert_eq!(report.exemptions(), 0);
    }
}

#[test]
fn justified_trailing_comment_exempts_only_its_alias() {
    let source = r#"let role = "assistant"; // layering-allow: assistant = standard model role"#;
    let report = scan_product_references(source, PRODUCTS);

    assert!(report.violations().is_empty());
    assert_eq!(report.exemptions(), 1);
}

#[test]
fn marker_text_inside_string_literal_does_not_exempt_product_table() {
    let source = r##"const PRODUCTS: (&str, &str) = ("assistant", r#"layering-allow: assistant = ordinary data"#);"##;
    let report = scan_product_references(source, PRODUCTS);

    assert_eq!(report.exemptions(), 0);
    assert!(
        messages(source)
            .iter()
            .any(|message| message.contains("ra-assistant"))
    );
}

#[test]
fn crate_identifiers_can_never_be_exempted() {
    let source = "let _ = ra_coding::Thing; // layering-allow: coding = protocol term";
    let report = scan_product_references(source, PRODUCTS);

    assert!(
        report
            .violations()
            .iter()
            .any(|violation| violation.message().contains("cannot be exempted"))
    );
    assert_eq!(
        report.exemptions(),
        0,
        "there is no quoted alias to consume"
    );
}

#[test]
fn exemption_does_not_hide_a_different_product_on_the_same_line() {
    let source = r#"let pair = ("assistant", "coding"); // layering-allow: assistant = model role"#;
    let report = scan_product_references(source, PRODUCTS);

    assert_eq!(report.exemptions(), 1);
    assert_eq!(report.violations().len(), 1);
    assert!(report.violations()[0].message().contains("ra-coding"));
}

#[test]
fn empty_unknown_and_malformed_markers_are_rejected() {
    let cases = [
        r#"let role = "assistant"; // layering-allow:"#,
        r#"let role = "assistant"; // layering-allow: assistant ="#,
        r#"let role = "assistant"; // layering-allow: unknown = reason"#,
        r#"let role = "assistant"; // layering-allow: assistant reason"#,
    ];

    for source in cases {
        let report = scan_product_references(source, PRODUCTS);
        assert!(
            report
                .violations()
                .iter()
                .any(|violation| violation.message().contains("marker")),
            "invalid marker was accepted: {source}"
        );
        assert_eq!(report.exemptions(), 0);
    }
}

#[test]
fn unused_marker_is_rejected_instead_of_inflating_the_count() {
    let source = "let role = 1; // layering-allow: assistant = stale exception";
    let report = scan_product_references(source, PRODUCTS);

    assert_eq!(report.exemptions(), 0);
    assert!(
        messages(source)
            .iter()
            .any(|message| message.contains("unused"))
    );
}

#[test]
fn comment_markers_are_lexed_outside_normal_and_raw_strings() {
    let source = concat!(
        "let normal = \"// layering-allow: assistant = string data\";\n",
        "let raw = r#\"// layering-allow: assistant = raw data\"#;\n",
        "let role = \"assistant\"; // layering-allow: assistant = model role\n",
    );
    let report = scan_product_references(source, PRODUCTS);

    assert!(report.violations().is_empty());
    assert_eq!(report.exemptions(), 1);
}

#[test]
fn full_line_and_nested_block_comments_do_not_create_product_references() {
    let source = concat!(
        "// \"assistant\" and ra_assistant are documentation\n",
        "/* outer \"coding\" /* nested ra_coding */ still comment */\n",
        "let value = 1;\n",
    );
    let report = scan_product_references(source, PRODUCTS);

    assert!(report.violations().is_empty());
    assert_eq!(report.exemptions(), 0);
}

#[test]
fn quote_character_literals_do_not_turn_following_comments_into_code() {
    let cases = [
        "let quote = '\"';\n// mentions ra_coding as documentation",
        "let quote = b'\"';\n// mentions ra_assistant as documentation",
        "let chars = ('x', '\\n', '\\'', '\\u{4e2d}', '中');\n// mentions ra_coding",
    ];

    for source in cases {
        let report = scan_product_references(source, PRODUCTS);
        assert!(
            report.violations().is_empty(),
            "character literal shifted lexer state: {source}"
        );
        assert_eq!(report.exemptions(), 0);
    }
}

#[test]
fn character_literals_preserve_exemptions_and_lifetime_tokens() {
    let source = concat!(
        "fn borrow<'a>(value: &'a str) -> &'a str { value }\n",
        "'retry: loop { break 'retry; }\n",
        "let quote = '\"';\n",
        "let role = \"assistant\"; // layering-allow: assistant = model role\n",
    );
    let report = scan_product_references(source, PRODUCTS);

    assert!(report.violations().is_empty());
    assert_eq!(report.exemptions(), 1);
}

#[test]
fn supplied_product_catalog_drives_identifiers_aliases_and_markers() {
    let products = &["ra-research"];
    let exempted = scan_product_references(
        r#"let role = "research"; // layering-allow: research = protocol role"#,
        products,
    );
    let identifier = scan_product_references("let _ = ra_research::Runner;", products);

    assert!(exempted.violations().is_empty());
    assert_eq!(exempted.exemptions(), 1);
    assert!(
        identifier
            .violations()
            .iter()
            .any(|violation| violation.message().contains("ra_research"))
    );
}
