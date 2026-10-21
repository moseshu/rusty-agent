# Function tools

`#[tool]` preserves a normal Rust function and adds `function_name::tool()`.
This example registers a synchronous line counter and an asynchronous calculator
with injected application context, parameter descriptions, an explicit default,
and fallible output. It uses the existing Runner and shipped Responses provider.

Dependencies: `ra-core`, `ra-macros`, `ra-runtime`, `ra-model` with default `openai`,
`async-trait`, `serde_json`, and `tokio`. See [Cargo.toml](Cargo.toml) and
[the runnable source](src/main.rs).

Run the deterministic offline model/tool/result round trip:

```bash
cargo run -p custom_tools -- --check
```

It prints the line counter's schema, invokes `multiply` through the Runner with
its default second factor, and checks the final answer and application counter.
This path needs no credentials or network.

For a live run, set `OPENAI_API_KEY` and `RA_MODEL` to a model available at your
endpoint, then run `cargo run -p custom_tools`. `OPENAI_BASE_URL` selects a custom
Responses endpoint and `RA_REQUEST` overrides the question. Live runs consume the
configured provider; they are not part of the offline check.

See function tool contracts for callbacks,
policies, argument semantics, supported types, and remaining upstream gaps.
Handwritten `Tool` implementations remain available for custom execution behavior.
