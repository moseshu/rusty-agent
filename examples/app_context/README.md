# Application context and lifecycle callbacks

Share a typed application object with dynamic instructions and a function tool through
`with_app_context(Arc<T>)`. The instruction callback borrows it across an await; the
tool selects a project name to return. The runner never serializes the context into
model input. This example verifies that a local account marker stays out of both HTTP
requests while the explicitly selected language and project name reach the model.

Known contract deviation (D-003): the current `dynamic_instructions_async_fn` returns
`ResolvedPrompt`, which the runtime lowers to a trailing **user** message. Python's dynamic
instructions become system instructions. This changes role priority and is not required
by Rust ownership or async execution. The example shows the currently implemented API;
it does not establish parity, and its offline check pins the current trailing-user placement
so that the fix tracked by task R4-11a must update it deliberately. See the [open deviation list](../../Docs/Rusty_Agent_Framework_Development_Plan.md#参考优先级移植记录与偏差状态).

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
