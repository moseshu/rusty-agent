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

# Core Architecture Discipline

- Treat `openai-agents-python` as the primary reference contract for provider-neutral `ra-core`
  concepts.
- Prefer upstream parity before new abstractions: preserve public API shape, field meaning,
  defaults, merge behavior, lifecycle semantics, and relevant test cases for model settings,
  model input/output items, tool contracts and registration, usage, and model/provider resolution.
- Adapt the representation only where Rust ownership, async execution, persistence, or type-system
  requirements make a direct port unsuitable. Document every material semantic deviation and its
  concrete reason.
- Keep provider wire formats, provider-only parameters and constraints, hosted-tool implementations,
  and pricing policy in adapters or runtime layers rather than expanding the provider-neutral core.
