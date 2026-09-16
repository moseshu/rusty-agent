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

# Porting Over Redesign

- When a capability already exists in `openai-agents-python` or in `codex`, port it. Read that
  implementation first and carry over its public surface, field meanings, defaults, merge order,
  errors, ownership and recovery semantics. Do not write a separate design for something upstream
  already implements, and do not let a stricter rule of our own stand in for the ported contract.
- `openai-agents-python` is the framework source; `codex` is the source for local implementations
  and coding-product behavior. Where both cover the same ground, the framework contract wins and the
  codex mechanism supplies the implementation.
- Anything beyond the ported contract is an extension: it belongs in explicitly selected product
  configuration or an adapter, accepted and reported separately, and it never becomes a
  precondition of the port.
- OpenAI's own account machinery is out of scope and is not ported: ChatGPT or OpenAI sign-in,
  subscription and backend authentication, and the default-OpenAI-client key plumbing
  (`set_default_openai_key` / `set_default_openai_client`) have no counterpart here. This exclusion
  covers account identity only. Sandbox-level authorization — path grants, mount credential
  acknowledgements, ambient-authority policy — is a sandbox mechanism and is still ported.
