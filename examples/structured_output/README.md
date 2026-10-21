# Typed structured output

Declare a Rust type with `Deserialize`, `Serialize`, and `JsonSchema`, then install it with
`AgentSpec::builder().output_type::<Review>()?`. The runner validates and decodes before delivery.
`result.final_output_as::<Review>()?` borrows that decoded object without parsing the final text.

Dependencies: `ra-core`, `ra-model` with default `openai`, `ra-runtime`, `async-trait`,
`schemars`, `serde`, and `tokio`. See [Cargo.toml](Cargo.toml) and [the runnable source](src/main.rs).

```bash
cargo run -p structured_output -- --check
cargo run -p structured_output -- --check --stream
```

These offline checks cover a valid typed object, invalid JSON, and a field with the wrong type.
Live runs use the shipped OpenAI Responses adapter:

```bash
export OPENAI_API_KEY="your-api-key"
export RA_MODEL="your-available-model"
# Optional: export OPENAI_BASE_URL="https://your-endpoint/v1"
cargo run -p structured_output
```

For sequences or scalar types, the wire object contains `response`; the returned Rust value is
unwrapped. Use `OutputSchema::for_type_with_strict::<T>(false)` for open maps or loose schemas.
The `ra_core::output::OutputSchema` rustdoc describes strictness, wrapping, and validation.
