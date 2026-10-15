# Kernel extension gate

This offline example supplies its own `Tool`, `Model`, and `ModelResolver`. It proves
that a third party can build an agent with the kernel alone. Start with
[basic_agent](../basic_agent/README.md) when using an existing provider.

```bash
cargo run -p minimal_agent
```

No credentials or environment variables are needed. Dependencies: `ra-core`,
`ra-runtime`, `async-trait`, `serde_json`, and `tokio`. There are no optional features.
The [manifest](Cargo.toml) intentionally has exactly two framework dependencies;
`cargo xtask layering` enforces that boundary. See [the runnable source](src/main.rs).
