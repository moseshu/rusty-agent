# MCP clients and servers

Status: the minimum tool integration and the stdio server follow-up are implemented
and self-checked; awaiting review. The shared session now supports tools, prompts,
and resources. Other MCP roadmap slices remain separate.

The framework contract was read from local `openai-agents-python`
`f53c62451c7f34c15f0c87fa745e05404025420f`. The implementation reference was
local Codex `815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052`. The SDK dependency is the
published `rmcp = 3.5.1`; Codex uses that version from git revision
`4048ac40e958fb1a7d614617a2ae8d26666ee2fa`. This slice uses the SDK's initialize
lifecycle and does not claim Codex's optional modern-protocol discovery/recovery
path or its git-only changes.

## Using a server

Enable `ra-mcp/http` for Streamable HTTP; stdio is enabled by default.
The common session and in-process server also compile with default features off.

```rust,no_run
use std::sync::Arc;
use ra_core::{agent::{AgentId, AgentSpec}, tool::Tool};
use ra_mcp::{
    client::{McpClient, McpClientOptions, stdio::McpStdioParams},
    filter::ToolFilter,
    tool::{McpDiscoveryOptions, discover_tools},
};

async fn configured_agent() -> ra_core::error::Result<()> {
    let client = McpClient::connect_stdio(
        "documents",
        McpStdioParams::new("my-mcp-server")
            .with_args(["--workspace".into(), "/workspace".into()]),
        McpClientOptions::default(),
    ).await?;
    // Discover the complete server set together, so names are checked across servers.
    let tools = discover_tools(
        &[(client.clone(), ToolFilter::default())],
        &McpDiscoveryOptions::new(),
    ).await?;
    let _agent = AgentSpec::builder()
        .id(AgentId::new("assistant"))
        .name("Assistant")
        .tools(tools.into_iter().map(|tool| tool as Arc<dyn Tool>))
        .build()?;
    // Keep the client alive through the run, then close its owned transport.
    client.cleanup().await?;
    Ok(())
}
```

For HTTP, use `McpClient::connect_http` with `McpHttpParams::new(url)`,
`.with_headers(...)`, and optionally `.with_bearer_token_env_var(name)`.
The environment token overrides static Authorization. This initialize-based transport
follows bounded redirects, matching Codex's legacy HTTP path. Codex's modern protocol and
agent-plugin redirect policies are outside this slice. Stdio inherits the Codex core
environment allowlist and overlays explicit `with_env` values. It does not inherit
the host's entire environment. Stderr is discarded in this adapter.

`McpDiscoveryOptions` selects the naming contract. The default is Python's: bare original
MCP names, with a name repeated across servers rejected. `McpToolNaming::ServerPrefixed` is
Python's `include_server_in_tool_names` (`mcp_{server}__{tool}`, avoiding
`with_reserved_tool_names`). Both work with every model adapter. `McpToolNaming::Namespaced`
selects Codex's `mcp__{server}` namespaces, an extension for `OpenAI` Responses only; the Chat
Completions and Anthropic adapters refuse namespaced tools, as the Python SDK does.

`InProcessToolServer::connect(name, handler, options)` accepts the official SDK's
`ServerHandler`, exposed through `ra_mcp::rmcp`. `server.client()` provides the same
initialized client used above; `server.cleanup()` closes both sides and joins the
server task. Its bounded memory stream performs no OS IPC or networking. The
host tool registration facade and typed registration helpers remain follow-up work.

## Reusable stdio lifecycle

`McpServer` follows Python's server protocol. `McpServerStdio` stores launch configuration
before connection, and serializes connect/cleanup on its own lifecycle lock. Requests never
wait on that lock: during an initialize they fail at once with "not initialized", as Python's
unset session does. Repeated connect while alive is idempotent. A failed or cancelled
initialize leaves no published session. As in Python's `finally`, cleanup always clears the
session and logs non-HTTP teardown failures instead of raising them; a retained `McpClient`
handle still reports them from its own `cleanup()`. Connect discards a dead session the same
way, so a failed transport never blocks reconnecting. Python lets cleanup close the exit stack
under a pending connect; Rust cannot close a future it does not own, so cleanup cancels the
active and queued connects, which drop their transports. A cancelled scope covers the wait for
the lifecycle lock as well as initialize. It stays cancelled until every overlapping cleanup
finishes or is dropped; connects requested during cleanup also return the cancellation error.
Callers can reconnect after cleanup finishes. After cleanup, connect creates a new
child and session; previously discovered tools keep the closed original session.

```rust,no_run
use ra_mcp::{McpServer, client::{McpClientOptions, stdio::{McpServerStdio, McpStdioParams}}};

async fn prompts_and_resources() -> ra_core::error::Result<()> {
    let server = McpServerStdio::new(McpStdioParams::new("my-mcp-server"))
        .with_name("documents")
        .with_options(McpClientOptions::default());
    server.connect().await?;
    let client = server.client()?;
    let _initialized = client.server_initialize_result();
    let _prompts = server.list_prompts().await?;
    let _prompt = server.get_prompt("review", None).await?;
    let page = server.list_resources(None).await?;
    if let Some(cursor) = page.next_cursor {
        let _next_page = server.list_resources(Some(cursor)).await?;
    }
    let _templates = server.list_resource_templates(None).await?;
    let _resource = server.read_resource("memo://documents/readme").await?;
    server.cleanup().await?;
    Ok(())
}
```

Prompt lists aggregate all pages, retain the first page's metadata and clear the final
cursor. Resources and resource templates return exactly one page and preserve its cursor
and metadata. Reading keeps text/blob resources, URI and MIME metadata in SDK result types.
All operations share timeouts, credential-safe transport errors and cancellation with tools.
The SDK's `ServerPeerInfo` is the retained initialize view; it represents server identity
as optional to also accommodate discovery. No protocol types enter `ra-core`.

Stdio params add `with_encoding(label)` and `with_encoding_error_handler(...)`. Defaults
are Python's UTF-8 and strict. `McpEncodingErrorHandler::{Strict, Ignore, Replace}` applies
in both directions. UTF-8, ASCII, Latin-1 and UTF-16 (including endian-specific and BOM forms)
are supported; other labels use `encoding_rs`'s registry. This is a Rust adaptation:
Python's arbitrary registered codecs are unavailable, and unsupported labels fail before
spawning. Labels are first normalized and resolved with Python's alias table for the
ASCII, Latin-1, UTF-8 and UTF-16 codecs (so `l1`, `cp819`, `iso8859_1`, `646` or `u16` keep
Python's meaning); WHATWG would map most ASCII/Latin-1 aliases to Windows-1252. Other legacy
codecs follow `encoding_rs` labels and mappings. Two UTF-16 details: decoding follows Python's
incremental decoder (empty output is valid, one truncated byte follows the error handler, two
bytes without a BOM always fail); encoding writes a single stream BOM, whereas Python's
per-message `str.encode("utf-16")` prefixes every line with one, which puts U+FEFF before
each later JSON value. Encoding failures retain no raw data in diagnostics. The owned byte
pump is needed because the SDK child transport fixes its byte stream to UTF-8; processes are
spawned with tokio directly and JSON-RPC stays in the official SDK.

`cached_tools()` defaults to `None`, matching Python with caching disabled. Cache/filter
policies and approval integration remain the dedicated follow-up tasks; the runner receives
no server lifecycle or MCP-specific policy. See [the stdio contract and validation record](../../Docs/R11-1_MCP_stdio_契约与验证.md).

## Contract and adaptations

| Behavior | Reference and implementation |
| --- | --- |
| SDK ownership | Codex `rmcp-client/src/rmcp_client.rs` and `in_process_transport.rs`; the SDK owns JSON-RPC, initialization, protocol framing and HTTP session headers. One shared client implements discovery/calls for every transport. |
| Catalog bounds | Python defaults: no automatic page, item, cursor-byte or overall deadline limit; repeated cursors are rejected and no partial listing is returned. `with_max_list_pages` ports Python's optional positive page bound. `with_codex_catalog_limits()` explicitly selects Codex `codex-mcp/src/pagination.rs`: 2048 items, 100 pages, 64 KiB cursors and one 30 s overall deadline. Empty cursors are valid. Resource pagination remains explicit and is unaffected. |
| Timeouts | Python `client_session_timeout_seconds = 5` bounds initialize and each tool, prompt or resource request. `with_session_timeout(None)` or `with_session_timeout(Duration::ZERO)` disables that deadline, matching Python's `None` and zero semantics. An explicitly configured catalog deadline still applies, and Codex's per-server tool timeout may replace the session timeout for calls. The client owns these timers so protocol cancellation can be sent before the timeout result returns. |
| Naming | Default and `ServerPrefixed`: Python `MCPUtil.get_all_function_tools` and `_build_prefixed_tool_name_overrides`, checked against names produced by running upstream; duplicate names are reported with Python's escaped `repr`. `Namespaced`: Codex `codex-mcp/src/tools.rs`, retaining its 128-byte combined limit. The necessary adapter adaptation also bounds each namespace and function name to 64 bytes so discovered tools can pass the existing Responses request validation. Namespace shortening uses the server identity, shared across its tools, and avoids literal names that resemble generated hashes. This adaptation only affects the explicitly selected namespaced extension. Original protocol names and client handles remain unchanged. |
| Frozen routing | Discovered `Arc<McpTool>` objects retain their exact client and raw tool name, bound by server position as Python binds a server object, so repeated server names still route correctly. They enter the existing runtime action surface; no second dispatch registry or live name lookup is added. A declaration the neutral tool contract cannot carry is skipped with a warning, as Codex does. |
| Filtering | Codex allowlist followed by denylist, using raw names. Absent allowlist allows all; an empty explicit allowlist allows none. Explicit UI visibility without `model` becomes Hidden, and remains available to host invocation. |
| Scheduling | Codex handler: `read_only_hint == Some(true)` permits Parallel; missing/false hints use Exclusive. Hints do not grant sandbox permissions. Server-wide parallel override is outside this slice. |
| Exposure | Codex `core/src/mcp_tool_exposure.rs`: deferred when tool search is selected, advertised otherwise. Current Codex's 8 KiB/64 KiB spec budgets apply to agent plugins; they are not imposed globally on this provider-neutral adapter. |
| Schema | Python `MCPUtil.to_function_tool` with strict conversion off: an absent `properties` becomes `{}`, and Codex's null `properties` also does. `ToolSchema::loose` otherwise preserves the original MCP schema. |
| Metadata | Python `_mcp_tool_metadata.py`: explicit `title` precedes `annotations.title`; the UI description remains the original long description, while the model description falls back from description to resolved title. The declaration's `_meta` is sent as the call's request `_meta`, as Python's static tool meta. Namespaced tools describe their namespace with the server's initialize instructions, as Codex does. URL-derived names remove userinfo, query and fragment as in Python `mcp/_logging.py`; raw server routing identity remains host-only. |
| Results | Python `invoke_mcp_tool`, default `use_structured_content=false`: content takes precedence over structuredContent, including `isError=true`. A server error result remains an actionable tool output, not a run-stopping framework error. The raw error flag is available to callers of `McpClient::call_tool`; the neutral `ToolOutput` has no equivalent flag. |
| Media adaptation | Text and images map directly. The requested extension maps embedded text/blob resources to inline File blocks (URI retained as filename). Audio uses the existing neutral Audio block. Resource links, including HTTP links, follow Python's JSON-text fallback, preserving the URI and metadata without treating resource identifiers as downloadable files. Any other block also takes that fallback. Empty content becomes one empty text block because the neutral result type forbids an unanswered call. Embedded-resource MIME metadata has no separate field in neutral `FileSource` and is not retained there. |
| Failure handling | Invalid arguments, transport failures, and SDK deadlines become typed tool errors with model-visible failure policy. A JSON-RPC error response keeps the server's code and message, as Python's `MCPError`; `-32602` maps to invalid input. Raw transport error sources are discarded to avoid retaining credential-bearing URLs/headers in Display, Debug or error chains. Retry policy is follow-up work. |
| Cancellation | Python's outer-cancellation propagation and Codex's cancellable SDK requests: dropped tool/list futures send a bounded best-effort protocol cancellation. Normal timeout uses the SDK's cancellation notification. Remote cancellation remains cooperative. In-process cleanup also cancels the owned server token, joins its task, and reports unconfirmed cleanup. |
| Cleanup | Client clones share closure. Normal client cleanup is idempotent; SDK close timeout or worker failure is reported by `McpClient::cleanup` and later client cleanup does not claim confirmed success. The configured `McpServer` cleanup follows Python instead: it always clears its session and logs such failures. Stdio byte I/O is transcoded around the SDK's `AsyncRwTransport`; its owner closes the pipes, waits up to 3 s, then kills and reaps the child, following the SDK child transport's shutdown mechanism. A shared failure flag carries transcoder/shutdown errors that the SDK otherwise only logs. Full Codex process-group/executor launcher behavior, sandbox authorization, and lifecycle recovery are not implemented by this slice. |

No MCP transport or provider wire types were added to `ra-core`. Approval
configuration, OAuth, cache invalidation/dynamic filtering,
connection pools/lazy startup, manager workers and runtime control remain later
slices. The published `sse` feature and its empty module remain as deprecated no-ops so
dependents keep building; they add no transport.

## Validation and review record

`cargo test --manifest-path tests/Cargo.toml -p it-mcp` exercises real SDK memory,
stdio child-process and loopback HTTP servers, direct/deferred Runner runs under bare and
namespaced names, and request lowering through the Responses, Chat Completions and Anthropic adapters.
Tests cover multi-page discovery (including an empty cursor), cycle/page/item/cursor
bounds, original-name routing across colliding servers, strictness, filters,
visibility, annotation defaults, metadata precedence, media bytes/order, error
content, credential-safe diagnostics, cancellation, timeouts and idempotent cleanup.
The HTTP check verifies static request headers and environment-token precedence; a missing token is also tested.

Self-review read the upstream implementation and relevant tests:

- Python `src/agents/mcp/{server,util,_logging}.py`, `_mcp_tool_metadata.py`;
  `tests/mcp/test_mcp_util.py`, `test_mcp_pagination_integration.py`,
  `test_server_errors.py`.
- Codex `codex-mcp/src/{tools,pagination}.rs`, `pagination_tests.rs`,
  `connection_manager_tests.rs`, `connection_manager/tool_catalog.rs`;
  `rmcp-client/src/{rmcp_client,in_process_transport,utils,stdio_server_launcher}.rs`;
  `core/src/tools/handlers/mcp.rs`, `core/src/mcp_tool_exposure.rs`.

Self-review corrected title precedence, preserved server error content, removed an
accidental whole-environment inheritance, made list requests cancellable, and
stopped treating an SDK close timeout as successful cleanup.

The first review (2026-10-16) found that namespaced names made every MCP tool unusable on the
Chat Completions and Anthropic adapters, and that schemas without `properties` were sent
unchanged. It also reported uncarried server error messages, Codex timeout defaults in place
of Python's, missing call `_meta`, hard failures on unknown content and on one unadaptable
declaration, missing namespace descriptions, and the removed `sse` feature. All of these are
fixed as described above. Python's prefixed names were checked against output from running
upstream `MCPUtil.get_all_function_tools`. The fixes await re-review; the roadmap status must
remain awaiting review.

The second review found three remaining issues: namespaced allocation could exceed the
Responses adapter's per-part name limit; Python's disabled session timeout could not be
represented; and unexpanded resource links were treated as file URLs. These are fixed with
regressions for long-name Responses requests and raw routing, stable namespace allocation,
delayed initialize/list/call with `None` and zero, finite deadlines and cancellation, and
resource-link JSON round trips through Responses and Anthropic requests. The fixes await
re-review; the roadmap status remains awaiting review.

Ported-file provenance is recorded in the top-level NOTICE and checked in both
directions by `cargo xtask ported-notice`.

Initial minimum-integration validation evidence (2026-10-16, before the stdio follow-up):

| Check | Result |
| --- | --- |
| MCP integration suite | 34 passed, 0 failed after both rounds of review fixes; includes both static HTTP authorization and environment-token override, legacy redirects, omitted/object arguments on all transports, upstream-generated prefixed names, long-name Responses request lowering, disabled/finite session deadlines, and resource-link preservation in Responses/Anthropic requests. Removing the `properties` insertion makes the provider test fail. |
| Runtime tool-search regression suite | 11 passed, 0 failed (first round; runtime code unchanged by the review fixes). |
| README Rust example | 1 compile-only doctest passed. |
| Workspace all targets/all features | `cargo check --workspace --all-targets --all-features --locked` passed. |
| MCP feature combinations | No default features, HTTP without stdio, the deprecated `sse` feature alone, and all features compiled. |
| Lints and format | Clippy for `ra-mcp` and `xtask`, all features, and all `it-mcp` targets with warnings denied; workspace and MCP-test rustfmt; rustdoc with warnings denied all passed. |
| Structural gates | Layering, no-inline-tests and ported-notice passed. |
| Public API | Only `ra_mcp::` baseline changes were accepted. The session and effective tool timeout accessors now return `Option<Duration>` so disabled deadlines are representable, and the published `sse` module is present again. The full gate still reports 34 existing public-field violations in session Git metadata and protocol thread-management records; those source files were not changed. |
| Dependency audit | The existing rustls 0.23.45 advisories were resolved by updating the lockfile to 0.23.46. The remaining failures are pre-existing `borrow-or-share 0.2.4` with unapproved MIT-0 license and yanked `zip 2.4.2`; license policy and sandbox archive dependencies were not changed. Bans and sources checks passed. |

These results are implementation and self-review evidence. They do not mark the
roadmap task DONE or replace independent review.

## Stdio follow-up validation (2026-10-16)

The final MCP suite has 54 passing tests, including 19 server/lifecycle/encoding cases
and one isolated cleanup-log test. Both README doctests, all workspace targets/features, MCP feature combinations,
Clippy with warnings denied, rustdoc with warnings denied, format, layering,
no-inline-tests and ported-notice passed. Public API reconciliation accepted only 35
new `ra_mcp::` items into the local baseline. The full public API gate still reports the
same 34 existing field violations; dependency audit still reports the existing MIT-0
policy rejection and yanked zip dependency. The new encoding dependencies add no audit
errors. See the linked stdio record for commands, scope and upstream adaptations.

The first review (2026-10-16) found that a failed stdio cleanup left the configured server
permanently unable to clean up or reconnect, that connect held the session slot so requests,
liveness checks and cleanup waited for a whole initialize, and that raising teardown errors
was stricter than Python. It also found Python ASCII/Latin-1 aliases resolving to
Windows-1252, undocumented UTF-16 differences with an empty-stream decode failure, and the
unused SDK child-process feature. All six are fixed and covered above; a fixture EOF bug that
made each transcoding test wait for the 3 s kill deadline was also fixed. The implementation
awaits re-review.

The second review found that cleanup could still wait behind a queued connect with no
session deadline, and that cleanup/reconnect warnings logged credential-bearing server
names verbatim. Both are fixed: shared cancellation covers active and queued connects,
cleanup scopes reopen connections only after all cleanup callers finish or are dropped,
and warnings use the existing Python-compatible safe server name. Regressions cover queued
connects, cancelled and overlapping cleanup, and captured credential-safe logs on cleanup
and reconnect. Original server identities remain unchanged. These fixes await re-review.
