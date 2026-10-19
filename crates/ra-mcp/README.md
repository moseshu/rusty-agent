# MCP clients and servers

Status: the minimum tool integration and reusable stdio server passed their full re-review
and initialize-error fix verification on 2026-10-17; both roadmap tasks are DONE. The
Streamable HTTP follow-up has also passed its recorded review. The shared session supports
tools, prompts and resources. Other MCP roadmap slices remain separate.

Tool filtering/cache passed re-review on 2026-10-18. The contract and validation record is in
[the filtering/cache notes](../../Docs/R11-4_MCP_工具过滤与缓存_契约与验证.md).
Metadata resolution, caching and tool-call display metadata passed re-review on 2026-10-18; see
[the metadata notes](../../Docs/R11-12_MCP_工具元数据解析_契约与验证.md).
Local tool approval and server tool guardrails passed review and re-review on 2026-10-19; see
[the approval notes](../../Docs/R11-5_MCP_工具审批_契约与验证.md).

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
use ra_core::agent::{AgentId, AgentSpec};
use ra_mcp::{
    McpServer,
    client::stdio::{McpServerStdio, McpStdioParams},
    filter::ToolFilter,
    resolver::McpToolResolver,
    tool::McpDiscoveryOptions,
};

async fn configured_agent() -> ra_core::error::Result<()> {
    let server = Arc::new(McpServerStdio::new(
        McpStdioParams::new("my-mcp-server")
            .with_args(["--workspace".into(), "/workspace".into()]),
    ).with_name("documents"));
    server.connect().await?;
    let resolver = Arc::new(McpToolResolver::new(
        vec![(server.clone(), ToolFilter::default())],
        McpDiscoveryOptions::new(),
    ));
    let _agent = AgentSpec::builder()
        .id(AgentId::new("assistant"))
        .name("Assistant")
        .tool_resolver(resolver)
        .build()?;
    server.cleanup().await?;
    Ok(())
}
```

For HTTP, use `McpClient::connect_http` with `McpHttpParams::new(url)`,
`.with_headers(...)`, and optionally `.with_bearer_token_env_var(name)`.
The environment token overrides static Authorization and any authentication hook.
Automatic redirects are disabled by default, matching Python's default factory; a custom
factory may explicitly change that policy. Codex's modern protocol discovery and recovery
remain outside this slice. Stdio inherits the Codex core
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

## Local tool approval

`McpClientOptions::with_require_approval` configures the server policy for stdio, HTTP and
in-process sessions. The default is `RequireApprovalSetting::Never`, as in Python.
Use `Always`, a tool-name map, or `dynamic_sync` / `dynamic` callbacks. JSON settings follow
Python's `require_approval` forms, including booleans and nested tool lists:

```rust
use ra_mcp::{approval::RequireApprovalSetting, client::McpClientOptions};
use serde_json::json;

let policy = RequireApprovalSetting::try_from(json!({
    "always": {"tool_names": ["delete_document"]},
    "never": {"tool_names": ["search"]}
}))?;
let options = McpClientOptions::default().with_require_approval(policy);
# Ok::<(), ra_core::error::Error>(())
```

Lists and maps match original protocol names; unlisted tools do not require approval.
Malformed values and conflicting lists fail during configuration. Callbacks receive the live
`RunContext`, the public agent captured during discovery, and an owned copy of the original
MCP declaration. Callback failures propagate; cancellation stops the callback through normal
dispatch. Discovery without an agent requires approval for callable policies without evaluating
them, matching Python's legacy conversion.

Runner uses the existing tool-approval interruption and `RunState::approve` / `reject` flow,
including remembered decisions and serialized recovery. MCP recipient binding is checked on
resume. Deferred tools retain their approval policy after tool search. Direct protocol calls
through `McpClient::call_tool` or `McpTool::invoke` remain host-controlled, as in Python;
runtime dispatch enforces approval when the model invokes a tool.

Server-wide `with_tool_input_guardrails` and `with_tool_output_guardrails` select the checks
that every discovered MCP tool declares. Both default to empty. They use the existing
`ToolGuardrailId` registry contract: register the corresponding implementations with
`RunConfig::with_tool_input_guardrails` / `with_tool_output_guardrails`. Each discovered tool
gets its own declaration lists, in server order; filters and other servers do not inherit them.
An unresolved ID fails preparation before the model call.

```rust
use ra_core::tool::ToolGuardrailId;
use ra_mcp::client::McpClientOptions;

let options = McpClientOptions::default()
    .with_require_approval(true)
    .with_tool_input_guardrails([ToolGuardrailId::new("validate_arguments")?])
    .with_tool_output_guardrails([ToolGuardrailId::new("validate_result")?]);
# Ok::<(), ra_core::error::Error>(())
```

The runtime's optional pre-approval input pass runs before an interruption. Approved calls
are checked again after serialized recovery, followed by invocation and output checks.
Remembered approval decisions skip the dynamic approval callback, while the call's guardrails
continue to apply. Raw protocol invocation remains outside the runtime guardrail chain.

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

`cached_tools()` is `None` before the first complete listing. It subsequently returns the
unfiltered snapshot even when cache reuse is disabled. Approval integration remains follow-up
work; the runner receives no server lifecycle or MCP-specific policy. See
[the stdio contract and validation record](../../Docs/R11-1_MCP_stdio_契约与验证.md).

## Tool filtering and caching

All transports use `McpClientOptions::with_cache_tools_list(bool)` (default false) and
`with_tool_filter(ToolFilter)`. Reusable servers receive these through `with_options`.
Clients and reusable servers also expose `set_tool_filter` and `set_cache_tools_list` for live
changes, matching Python's mutable attributes. These changes retain the unfiltered snapshot and
remain active after reconnect; a client's `options()` describes its initial connection options.
`ToolFilter::new(allowlist, blocklist)` retains the existing static constructor;
`create_static_tool_filter` accepts optional name lists and returns `None` when both are absent.
An explicit empty allowlist excludes everything, and blocking wins over allowing. Filters
inspect original protocol names before model-name allocation. They control discovery, rather
than granting or enforcing sandbox authorization on direct `call_tool` invocations.

`ToolFilter::dynamic_sync` accepts a fallible synchronous predicate; `ToolFilter::dynamic`
accepts a boxed asynchronous future that can borrow `ToolFilterContext`. The context exposes
`run_context()`, `agent()` and `server_name()`. The agent is a `RunAgent` public identity view;
it exposes neither sibling executables nor model settings. Dynamic predicates require both run and agent,
even for an empty catalog; contextless discovery reports a configuration error. Each predicate
receives an owned tool snapshot. A returned error excludes that tool, logs a generic diagnostic
without tool identity, server credentials or exception text, and continues with the remaining
tools. Rust panics are not predicate errors: return `Result<bool>` for recoverable failures.

```rust,no_run
use ra_core::context::RunContext;
use ra_mcp::{
    client::{McpClient, McpClientOptions},
    filter::ToolFilter,
    tool::{McpDiscoveryOptions, discover_tools_with_context},
};

fn options() -> McpClientOptions {
    McpClientOptions::default()
        .with_cache_tools_list(true)
        .with_tool_filter(ToolFilter::dynamic(|context, tool| Box::pin(async move {
            let admin = context.run_context().app_context::<bool>().copied().unwrap_or(false);
            Ok(admin || tool.name == "read")
        })))
}

async fn refresh(client: McpClient, context: &RunContext)
    -> ra_core::error::Result<()>
{
    let _tools = discover_tools_with_context(
        &[(client.clone(), ToolFilter::default())],
        &McpDiscoveryOptions::new(), Some(context), Some(context.agent()),
    ).await?;
    let _unfiltered = client.cached_tools();
    client.invalidate_tools_cache();
    Ok(())
}
```

Attach `McpToolResolver` with `AgentSpecBuilder::tool_resolver` for runner integration. Before
every ordinary or streamed model call, preparation supplies that step's real `RunContext`,
including current usage, host app state and the public agent identity. Handoffs use the target's
resolver and public identity; execution variants preserve or explicitly clear their resolver.
This boundary ports Python's `Agent.get_mcp_tools` through `ra_core::tool::ToolResolver`, so the
runtime kernel depends only on neutral tools rather than on this adapter.

Register configured servers as `Arc<dyn McpClientSource>` (coercion is automatic in the example).
The resolver captures each server's current session on every preparation, so cleanup/reconnect
is visible on the next turn or run without rebuilding the agent. An explicitly registered
`Arc<McpClient>` instead stays bound to that one session. Discovery results retain the captured
session for their model call; reconnects never silently retarget already advertised tools.

Normal preparation resolves MCP tools, local enablement, then handoffs. Only prefixed naming
first evaluates enabled handoff names for reservations, then evaluates handoffs again after
tools, as Python does. Approval resumption instead reuses one handoff snapshot for discovery.
MCP tools precede enabled local tools, matching Python's collision precedence. Collision
checks, tool search and action budgets all consume this same frozen inventory, which dispatch
retains through the model call. A later filter update affects the next preparation. Approval
resumption also resolves the current inventory and compares a persisted recipient binding;
MCP's credential-safe server name, raw tool name and server position cannot be replaced by a newly allocated name.
Recipient data stays in the control plane, is omitted from approval Debug output, and is never
sent to the model. Server approval policy configuration remains separate follow-up work.

Use `list_tools_with_context` on a client or `dyn McpServer` for raw protocol declarations,
and `discover_tools_with_context` for framework tools. The existing zero-context methods remain
available for static filters. A discovery tuple's filter is an additional explicit restriction
after the configured server filter; neither filtered result is stored in the cache.

Every successful complete listing publishes an unfiltered snapshot. Cache reuse requires an
enabled, clean, nonempty cache, following Python (empty lists are fetched again). Dynamic filters
run on every listing, including cache hits. Invalidation retains the last complete snapshot and
marks it dirty. A refresh started before an invalidation cannot publish; an older refresh cannot
overwrite a newer successful publication, but may publish if that newer refresh failed.
Pagination failure, timeout and cancellation never publish partial results. Refreshes are not
coalesced or serialized beyond the existing transport request serialization.

Snapshot values own their mutable fields; SDK schema `Arc`s use Rust's copy-on-write semantics,
so `Arc::make_mut` on a returned declaration cannot alter the cache. Clean cached schemas also
validate string-valued `required` names before `call_tool`, including when cache reuse is off;
dirty or absent caches skip this check, as Python does. Reusable servers retain their catalog
through cleanup/reconnect, as upstream does; invalidate explicitly when the server's tools change.
Retained clients and registered tools keep their original transport session.

`ToolFilter` now represents the static-or-callable union. Synchronous static inspection methods
`allows` and `filter_tools` move to `ToolFilterStatic`; construct that type directly or match
`ToolFilter::Static`. The existing `ToolFilter::new` and default discovery tuples still work.

## Tool display metadata

`McpTool::info().metadata()` returns an immutable `metadata::McpToolMetadata` with the original
long-form `description()` and resolved `title()`. The existing `McpToolInfo::title()` and
`description_for_ui()` delegate to this snapshot. Explicit `title` precedes `annotations.title`;
there is no tool-name fallback. Empty strings are absent, while whitespace is preserved.
Only `description_for_model()` falls back from the description to the resolved title and then
the empty string. The model schema carries that one description, without additional UI fields.
The title fallback is intentional upstream behavior when the long-form description is absent.

Metadata is resolved once for each declaration in a complete tools/list refresh, before filtering.
Cache hits, static/dynamic filtering, name allocation, and direct/deferred tool adaptation share
the immutable value. Tools retain their exact declaration and metadata after later refreshes.
Metadata uses the existing cache enablement, generation and publication rules; it has no separate
TTL, global name-keyed cache, or additional invalidation API. Configured servers retain their
complete catalog across cleanup/reconnect as Python does, including its metadata, until refresh
or invalidation. Newly discovered tools still bind to the new session.

`McpToolMetadata::from(&rmcp_tool)` resolves an SDK declaration. Raw JSON helpers and the
hosted collector are internal implementation details; they are not part of `ra_mcp`'s public API.

The runner copies a local function tool's schema description and `Tool::title()` onto the
created `RunItem`. The description already includes the MCP title fallback when needed;
`Tool::title()` defaults to `None` for ordinary tools. Both fields are optional, preserved by
`RunState` and session serialization, and visible in settled run stream items. They live on the
run-item envelope, separately from the raw tool call and `ModelInputItem`:

```rust
use ra_core::item::{CallId, ItemId, RunItem, RunItemKind, ToolCall};
use serde_json::json;

let item = RunItem::new(ItemId::new("call-item"), RunItemKind::ToolCall(
    ToolCall::new(CallId::new("call"), "search", json!({})),
)).with_tool_call_metadata(Some("Search docs".into()), Some("Search".into()));
assert_eq!(item.title(), Some("Search"));
let replay = serde_json::to_value(item.to_model_input().unwrap()).unwrap();
assert!(replay["data"].get("title").is_none());
assert!(replay["data"].get("description").is_none());
```

Hosted MCP output uses the Responses adapter's normalized `McpListTools` and `McpCall` items.
The runtime collects metadata from authoritative pre-step items, then current output,
keyed by `(server_label, name)`. Later entries replace whole metadata values. A hosted call's
description remains absent when the directory has no description; it does not use the local
function-tool fallback. Collection uses typed records, even when a model-input filter or server
continuation hides an earlier directory from the actual request. Provider catalogs retain their
complete `McpListTools::provider_data` for replay; only the private collector validates metadata
fields, without filtering declarations or supplying missing schemas. A provider-executed call
stores its output/error on that same record and schedules no local tool execution. It counts as
`ToolUse::Mcp` in the existing tracker, allowing `reset_tool_choice` to release a forced choice.
Custom call items do not inherit the tool declaration's description or title.

The Responses adapter owns wire conversion and replay of both hosted item kinds. Existing schema
and model-input projections never acquire the run item's display fields. The source contract,
Rust representation adaptations and validation are recorded in
[the metadata notes](../../Docs/R11-12_MCP_工具元数据解析_契约与验证.md).

## Reusable Streamable HTTP lifecycle

`McpServerStreamableHttp` shares stdio's lifecycle state and implements the same `McpServer`
protocol. `McpTransportConfig::{Stdio, StreamableHttp}` selects either transport through
`McpClient::connect_transport`. The default HTTP name is `streamable_http: <url>`; diagnostics
sanitize URL-derived names. Old client handles remain bound to their original session.

```rust,no_run
use ra_mcp::{McpServer, client::{McpClientOptions, http::{McpHttpParams, McpServerStreamableHttp}}};
use std::time::Duration;

async fn remote_server() -> ra_core::error::Result<()> {
    let params = McpHttpParams::new("https://example.test/mcp")
        .with_timeout(Duration::from_secs(5))
        .with_sse_read_timeout(Duration::from_secs(300))
        .with_terminate_on_close(true)
        .with_auth(|request| async move { Ok(request.bearer_auth("host-supplied-token")) })
        .with_http_client_factory(|builder| builder.user_agent("my-agent").build());
    let server = McpServerStreamableHttp::new(params)
        .with_options(McpClientOptions::default());
    server.connect().await?;
    let _session_id = server.session_id();
    let _tools = server.list_tools().await?;
    server.cleanup().await?;
    assert!(server.session_id().is_none());
    Ok(())
}
```

The native authentication hook runs for POST, GET and DELETE. Reqwest configures auth on
requests, so the factory receives a configured `ClientBuilder` while auth stays a request
hook. It can obtain credentials asynchronously; challenge flows and OAuth storage remain
separate. Static headers are applied first, then auth, then the explicit environment token.
Factory and auth errors discard their original sources before entering SDK diagnostics.
These hooks use reqwest 0.13 types; `ra_mcp::client::http::reqwest` re-exports that version.
Response media types are matched case-insensitively.

HTTP connect timeout defaults to 5 seconds and read inactivity timeout to 300 seconds.
Reqwest has no separate HTTPX-style write or connection-pool timeout; the shared MCP session
deadline bounds complete requests and defaults to 5 seconds. Disabling that deadline leaves
only the HTTP connect and read timers. This phase-timeout difference is a documented Rust
adaptation. HTTP session requests are serialized, including each catalog page; cancellation
releases the lock and sends the shared SDK cancellation notification.

Stateful cleanup sends DELETE by default and waits for the SDK's bounded shutdown; stateless
sessions and `with_terminate_on_close(false)` skip it. A 405 DELETE is accepted. Failed
initialization awaits deletion of an assigned session; as in Python's transport, a failed or
timed-out DELETE is logged and the connection error is returned. Cleanup transfers the client
and session ID into one task with a 5-second bound before awaiting it. Cancelling the waiter
detaches that task, so the same DELETE and asynchronous auth hook continue without a retry.
Dropping an initializing connect starts the same task because Rust Drop cannot await; this
fallback needs the Tokio runtime to stay alive. Cleanup always clears the visible session ID.

Initialized-notification failures remain errors by default.
`with_ignore_initialized_notification_failure(true)` selects Python v1's compatibility behavior
for that notification alone. The pinned Rust SDK exposes this boundary, so Python v2's
unsupported-option rejection has no counterpart here. HTTP status errors, invalid JSON/SSE,
auth failures, URL credentials, challenges and error bodies are sanitized before SDK logging.
A JSON-RPC error body is a server answer at any HTTP status, as in the MCP Python SDK; it keeps
its code and message and is correlated to the request. SSE validation applies only to message
events. Initialize filters control frames before the SDK's separate initialize reader, which
otherwise parses their data as JSON. Established streams retain their control frames for the
SDK connector. No transparent session recovery or tool replay is enabled. See the [HTTP contract and validation record](../../Docs/R11-2_MCP_HTTP_契约与验证.md).

## Contract and adaptations

| Behavior | Reference and implementation |
| --- | --- |
| SDK ownership | Codex `rmcp-client/src/rmcp_client.rs` and `in_process_transport.rs`; the SDK owns JSON-RPC dispatch, initialization and HTTP session lifecycle. A native HTTP boundary translates responses into SDK protocol types, without retaining credential-bearing HTTP errors. One shared client implements discovery/calls for every transport. |
| Catalog bounds | Python defaults: no automatic page, item, cursor-byte or overall deadline limit; repeated cursors are rejected and no partial listing is returned. `with_max_list_pages` ports Python's optional positive page bound. `with_codex_catalog_limits()` explicitly selects Codex `codex-mcp/src/pagination.rs`: 2048 items, 100 pages, 64 KiB cursors and one 30 s overall deadline. Empty cursors are valid. Resource pagination remains explicit and is unaffected. |
| Timeouts | Python `client_session_timeout_seconds = 5` bounds initialize and each tool, prompt or resource request. `with_session_timeout(None)` or `with_session_timeout(Duration::ZERO)` disables that deadline, matching Python's `None` and zero semantics. An explicitly configured catalog deadline still applies, and Codex's per-server tool timeout may replace the session timeout for calls. The client owns these timers so protocol cancellation can be sent before the timeout result returns. |
| Naming | Default and `ServerPrefixed`: Python `MCPUtil.get_all_function_tools` and `_build_prefixed_tool_name_overrides`, checked against names produced by running upstream; duplicate names are reported with Python's escaped `repr`. `Namespaced`: Codex `codex-mcp/src/tools.rs`, retaining its 128-byte combined limit. The necessary adapter adaptation also bounds each namespace and function name to 64 bytes so discovered tools can pass the existing Responses request validation. Namespace shortening uses the server identity, shared across its tools, and avoids literal names that resemble generated hashes. This adaptation only affects the explicitly selected namespaced extension. Original protocol names and client handles remain unchanged. |
| Frozen routing | Discovered `Arc<McpTool>` objects retain their exact client and raw tool name, bound by server position as Python binds a server object, so repeated server names still route correctly. They enter the existing runtime action surface; no second dispatch registry or live name lookup is added. A declaration the neutral tool contract cannot carry is skipped with a warning, as Codex does. |
| Filtering | Python static-or-callable filtering after the unfiltered catalog snapshot, with Codex's equivalent static allow/block membership. Missing dynamic context errors; per-tool predicate errors exclude the tool and log a credential-safe message. Explicit UI visibility without `model` becomes Hidden, and remains available to host invocation. |
| Caching | Python's default-disabled reuse, complete unfiltered snapshots, explicit dirty-generation invalidation, and successful-refresh publication order. Dynamic filters run again on cache hits. Clean snapshots validate required arguments before invocation. |
| Scheduling | Codex handler: `read_only_hint == Some(true)` permits Parallel; missing/false hints use Exclusive. Hints do not grant sandbox permissions. Server-wide parallel override is outside this slice. |
| Exposure | Codex `core/src/mcp_tool_exposure.rs`: deferred when tool search is selected, advertised otherwise. Current Codex's 8 KiB/64 KiB spec budgets apply to agent plugins; they are not imposed globally on this provider-neutral adapter. |
| Schema | Python `MCPUtil.to_function_tool` with strict conversion off: an absent `properties` becomes `{}`, and Codex's null `properties` also does. `ToolSchema::loose` otherwise preserves the original MCP schema. |
| Metadata | Python `_mcp_tool_metadata.py`: explicit `title` precedes `annotations.title`; the UI description remains the original long description, while the model description falls back from description to resolved title. The declaration's `_meta` is sent as the call's request `_meta`, as Python's static tool meta. Namespaced tools describe their namespace with the server's initialize instructions, as Codex does. URL-derived names remove userinfo, query and fragment as in Python `mcp/_logging.py`; raw server routing identity remains host-only. |
| Results | Python `invoke_mcp_tool`, default `use_structured_content=false`: content takes precedence over structuredContent, including `isError=true`. A server error result remains an actionable tool output, not a run-stopping framework error. The raw error flag is available to callers of `McpClient::call_tool`; the neutral `ToolOutput` has no equivalent flag. |
| Media adaptation | Text and images map directly. The requested extension maps embedded text/blob resources to inline File blocks (URI retained as filename). Audio uses the existing neutral Audio block. Resource links, including HTTP links, follow Python's JSON-text fallback, preserving the URI and metadata without treating resource identifiers as downloadable files. Any other block also takes that fallback. Empty content becomes one empty text block because the neutral result type forbids an unanswered call. Embedded-resource MIME metadata has no separate field in neutral `FileSource` and is not retained there. |
| Failure handling | Invalid arguments, transport failures, and SDK deadlines become typed tool errors with model-visible failure policy. A JSON-RPC error response keeps the server's code and message, as Python's `MCPError`; `-32602` maps to invalid input. Raw transport error sources are discarded to avoid retaining credential-bearing URLs/headers in Display, Debug or error chains. Retry policy is follow-up work. |
| Cancellation | Python's outer-cancellation propagation and Codex's cancellable SDK requests: dropped tool/list futures send a bounded best-effort protocol cancellation. Normal timeout uses the SDK's cancellation notification. Remote cancellation remains cooperative. In-process cleanup also cancels the owned server token, joins its task, and reports unconfirmed cleanup. |
| Cleanup | Client clones share closure. Normal client cleanup is idempotent; SDK close timeout or worker failure is reported by `McpClient::cleanup` and later client cleanup does not claim confirmed success. The configured `McpServer` cleanup follows Python instead: it always clears its session and logs such failures. Stdio byte I/O is transcoded around the SDK's `AsyncRwTransport`; its owner closes the pipes, waits up to 3 s, then kills and reaps the child, following the SDK child transport's shutdown mechanism. A shared failure flag carries transcoder/shutdown errors that the SDK otherwise only logs. Full Codex process-group/executor launcher behavior, sandbox authorization, and lifecycle recovery are not implemented by this slice. |

No MCP transport or provider wire types were added to `ra-core`. Local approval configuration
is described above. OAuth,
connection pools/lazy startup, manager workers and runtime control remain later
slices. The obsolete empty `sse` feature and module have been removed in the HTTP follow-up,
as selected by the roadmap; legacy HTTP+SSE servers are not supported. Streamable HTTP's
SSE response bodies remain supported through the SDK worker.

## Validation and review record

`cargo test --manifest-path tests/Cargo.toml -p it-mcp` exercises real SDK memory,
stdio child-process and loopback HTTP servers, direct/deferred Runner runs under bare and
namespaced names, and request lowering through the Responses, Chat Completions and Anthropic adapters.
Tests cover multi-page discovery (including an empty cursor), cycle/page/item/cursor
bounds, original-name routing across colliding servers, strictness, filters,
visibility, annotation defaults, metadata precedence, media bytes/order, error
content, credential-safe diagnostics, cancellation, timeouts and idempotent cleanup.
HTTP checks also cover stateful and stateless JSON/SSE responses, session IDs and reconnect,
auth/factory hooks, token precedence, redirects, serialization, read/session timeouts,
initialized-notification failure scope, failed/cancelled initialize cleanup, JSON-RPC errors at
non-2xx statuses, media-type case, SSE control frames and TRACE-level credential redaction.
A missing token is also tested.

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
upstream `MCPUtil.get_all_function_tools`. The fixes awaited re-review at that stage; their completion is recorded in the
2026-10-17 re-review and fix verification below.

The second review found three remaining issues: namespaced allocation could exceed the
Responses adapter's per-part name limit; Python's disabled session timeout could not be
represented; and unexpanded resource links were treated as file URLs. These are fixed with
regressions for long-name Responses requests and raw routing, stable namespace allocation,
delayed initialize/list/call with `None` and zero, finite deadlines and cancellation, and
resource-link JSON round trips through Responses and Anthropic requests. The fixes awaited
re-review at that stage; their completion is recorded below.

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
awaited re-review at that stage; the final conclusion is recorded below.

The second review found that cleanup could still wait behind a queued connect with no
session deadline, and that cleanup/reconnect warnings logged credential-bearing server
names verbatim. Both are fixed: shared cancellation covers active and queued connects,
cleanup scopes reopen connections only after all cleanup callers finish or are dropped,
and warnings use the existing Python-compatible safe server name. Regressions cover queued
connects, cancelled and overlapping cleanup, and captured credential-safe logs on cleanup
and reconnect. Original server identities remain unchanged. These fixes were verified in
the final re-review below.

## Minimum integration and stdio re-review (2026-10-17)

Reviewed the current implementation at `549317f` against the pinned Python and Codex
implementations and their relevant naming, pagination, prompt/resource, cancellation and
lifecycle tests. The earlier review fixes are present and their regressions pass. Rust
session ownership, encoding/BOM adaptations and explicitly selected Codex naming/catalog
extensions remain documented separately from the Python framework contract.

The re-review found one P2 issue: `McpClient::connect` maps every SDK initialize failure to
`initialize failed`, including `ClientInitializeError::JsonRpcError`. A server response
with code `-32602` and message `workspace configuration is unsupported` loses both the
code/message and its invalid-input classification. Python preserves the connection
exception after cleanup; Codex retains the typed handshake source. The request path in
this crate already maps protocol errors separately from credential-bearing transport
errors, so the same distinction is needed at initialize. Preserve the protocol code and
message while keeping raw transport errors sanitized.

Two temporary regression tests reproduced the loss through the in-process and real stdio
paths: `initialize_protocol_error_keeps_code_and_message_in_memory` and
`initialize_protocol_error_keeps_code_and_message_on_stdio`. Both failed with only
`initialize failed`. The temporary test source was retained outside the repository and
removed from the integration suite after reproduction; production code was not changed.

Validation before the initialize-error fix: 73 integration tests passed, with one child-process fixture skipped
by the parent harness (the provider tests execute it in a fresh process); three doctests
passed. Default/no-default/HTTP-only/all-feature checks, MCP and integration-test Clippy
with warnings denied, rustdoc with warnings denied, formatting, layering, no-inline-tests
and ported-notice passed. The public API gate, unified `cargo xtask test -p it-mcp --locked`
entry and `cargo deny check all` now also pass; their older failures above are historical.

The finding blocked completion at that stage. Its fix and the subsequent verification
are recorded below.

## Initialize-error fix verification (2026-10-17)

`McpClient::connect` now handles the SDK's `JsonRpcError` separately and reuses the
ordinary request error mapping. Server code/message and the invalid-input classification
for `-32602` survive initialization; other protocol codes remain execution failures.
Protocol diagnostic data and SDK sources are not retained. Other initialize failures
still use the original sanitized error, and session timeout and cleanup paths are unchanged.

The permanent `tests/it-mcp/tests/initialize.rs` regressions both failed before the fix
and pass afterward. Memory tests cover invalid params, internal error and a server-defined
code, retained messages, discarded diagnostic data and absent sources. The real stdio
fixture rejects initialize twice through one configured server; each attempt retains the
protocol error, publishes no client and allows cleanup and another connection attempt.
The full suite also retains its transport-error/log redaction, timeout and cleanup tests.

Fix review checked the changed mapping against Python's connection-error preservation,
Codex's typed handshake error and the SDK's separate protocol/transport error variants.
It confirms that the change resolves the sole open finding without retaining unsafe
transport errors or adding a public API or provider type to the core. Together with the
full re-review above, all blocking findings for the minimum integration and stdio tasks
are resolved; both are marked DONE on 2026-10-17.

The initialize-error fix verification passed 75 tests via `cargo xtask test -p it-mcp --locked`, with one parent-harness fixture
skip, and three doctests pass. Both Clippy scopes, all four MCP feature configurations,
rustdoc, formatting, layering, ported-notice, no-inline-tests and public-api pass again.
The reviewed fix was committed as `e8cf669`, based on `549317f`.

### HTTP initialize-error coverage follow-up (2026-10-17)

The HTTP fixture now has an initialize JSON-RPC error switch. Its regression covers JSON
and SSE responses, stateful and stateless sessions, invalid params, internal errors and a
server-defined code. It checks code/message/classification, discarded diagnostic data and
sources, unpublished failed sessions, DELETE of assigned sessions, idempotent cleanup and
a successful connection after the rejection is removed.

The new test exposed an SSE-specific gap: the SDK's initialize reader returns only success
responses and drains JSON-RPC errors until EOF. The native boundary now reads the first
terminal success or error message and supplies it to the SDK as its typed message, matching
Python's SSE handling. It still filters control frames, validates message payloads and
enforces the existing event size and timeout bounds; established streams are unchanged.
The regression failed before this adapter fix and passes afterward. The full MCP suite
now passes 76 tests with the same one parent-harness fixture skip. This follow-up does not
change the completed roadmap task statuses.
