// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Redaction of bearer tokens and its false-positive boundaries.
//!
//! Ported from the unit tests in Codex `codex-rs/secrets/src/sanitizer.rs`.

use ra_secrets::redact_secrets;

#[test]
fn redacts_supported_bearer_tokens() {
    let cases = [
        (
            "Bearer abcde+fghijklmnopqrstuvwxyz012345",
            "Bearer [REDACTED_SECRET]",
        ),
        (
            "Bearer abcdefghijklmnop+secret_suffix",
            "Bearer [REDACTED_SECRET]",
        ),
        (
            "Bearer sk-abcdefghijklmnopqrst+secret_suffix",
            "Bearer [REDACTED_SECRET]",
        ),
        (
            "Bearer AKIAABCDEFGHIJKLMNOP/~secret_suffix",
            "Bearer [REDACTED_SECRET]",
        ),
        (
            "Bearer AbcdefghijklMN09._~+/-==; echo done",
            "Bearer [REDACTED_SECRET]; echo done",
        ),
        (
            "authorization: bEaReR\tabcdefghijklmnop",
            "authorization: Bearer [REDACTED_SECRET]",
        ),
        ("Bearer   abcdefghijklmnop", "Bearer [REDACTED_SECRET]"),
    ];

    for (input, expected) in cases {
        assert_eq!(redact_secrets(input.to_string()), expected);
    }
}

#[test]
fn avoids_bearer_false_positives() {
    let cases = [
        "Bearer of good news",
        "Bearer abcdefghijklmno",
        "NotABearer abcdefghijklmnop",
        "Bearerabcdefghijklmnop",
        "Bearer\nabcdefghijklmnop",
        "Bearer\u{a0}abcdefghijklmnop",
        "Bearer abcdefghijklmno\u{212a}",
    ];

    for input in cases {
        assert_eq!(redact_secrets(input.to_string()), input);
    }
}
