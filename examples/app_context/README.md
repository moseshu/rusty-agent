# Application context and lifecycle callbacks

Share a typed application object with dynamic instructions and a function tool through
`with_app_context(Arc<T>)`. The instruction callback borrows it across an await; the
tool selects a project name to return. The runner never serializes the context into
model input. This example verifies that a local account marker stays out of both HTTP
requests while the explicitly selected language and project name reach the model.

The `dynamic_instructions_async_fn` callback returns `ResolvedPrompt`; its complete text
becomes system instructions on each model turn, matching Python's dynamic instructions.
The offline check asserts the Responses `instructions` field and rejects a duplicate in
conversation input. See the [instruction contract](../../Docs/Dynamic_Instructions_Contract.md).

```bash
cargo run -p app_context -- --check
```

`Events` is a minimal `LifecycleHook` observer for agent start/end and tool start/end.
The check verifies each callback fires once. Hook errors propagate; an interrupted
run has no agent-end callback because it has not delivered an answer.

Dependencies: `rusty-agent` with default `openai`, `tokio` with `net`, `serde_json`,
and `async-trait`. See [Cargo.toml](Cargo.toml), [src/main.rs](src/main.rs), and the
shared [provider setup / HTTP fixture](../support.rs).

For a live run:

```bash
export OPENAI_API_KEY="your-key"
export RA_MODEL="your-available-model"
# Optional: export OPENAI_BASE_URL="https://your-endpoint/v1/"
cargo run -p app_context
```

Use `with_context(T)` when the request owns a fresh value. It stores exactly `T`:
passing `Arc<T>` there stores `Arc<T>`, so `app_context::<T>()` will not find it.
Use `with_app_context` for an already shared object. Context is a live dependency;
reattach it when restoring a serialized checkpoint.
