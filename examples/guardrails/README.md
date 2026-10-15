# Guardrails

Attach `InputGuardrail` and `OutputGuardrail` implementations to an agent. This
example allows a greeting, rejects the `BLOCK_INPUT` marker before any model call,
and rejects an answer containing `BLOCK_OUTPUT` before delivery. Marker checks are
small tutorial fixtures, not a content-classification policy.

```bash
cargo run -p guardrails -- --check
```

`BlockInput` explicitly sets `run_in_parallel` to false. The framework default is
true, matching Python: the input check races the first model call. Select blocking
behavior when the check must finish before model or tool work begins. Output tripwires
return `guardrail.output`; input tripwires return `guardrail.input`. A checker failure
is a separate error, and neither tripwire automatically asks the model to retry.
The offline check validates both errors and successful verdict records, with exactly
two HTTP requests, proving the blocked input does not reach the provider.

Dependencies: `rusty-agent` with default `openai`, `tokio` with `net`, `serde_json`,
and `async-trait`. See [Cargo.toml](Cargo.toml), [src/main.rs](src/main.rs), and the
shared [provider setup / HTTP fixture](../support.rs).

For a live run:

```bash
export OPENAI_API_KEY="your-key"
export RA_MODEL="your-available-model"
# Optional: export OPENAI_BASE_URL="https://your-endpoint/v1/"
cargo run -p guardrails
```

The live model must follow the request to repeat the marker; unexpected model behavior
fails the demonstration instead of being reported as a passed check.
