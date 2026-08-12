# Rust Source Conventions

Apply these rules to every Rust source file (`.rs`), including tests.

- Use English, ASCII identifiers for functions, structs, enums, traits, types, modules, constants,
  variables, and fields. Follow Rust naming conventions, such as `snake_case` for functions and
  `UpperCamelCase` for types.
- Write all comments and Rust doc comments in English.
- Describe behavior and rationale directly. Do not use task or milestone labels such as `R17`,
  `R3`, or `R12` in Rust comments or doc comments.
- Localized text is allowed only when it is intentionally part of a user-facing message or test
  fixture; it must not be used as an identifier or a comment.
