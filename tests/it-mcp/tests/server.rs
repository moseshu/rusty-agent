//! Server lifecycle and prompt/resource contracts ported from the Python SDK tests.
#[path = "../support/mod.rs"]
mod support;
use ra_mcp::{
    McpServer,
    client::{
        McpClient, McpClientOptions,
        stdio::{McpEncodingErrorHandler, McpServerStdio, McpStdioParams},
    },
    in_process::InProcessToolServer,
};
use rmcp::model::*;
use serde_json::{Value, json};
use std::{future::Future, num::NonZeroUsize, sync::Arc, task::Poll, time::Duration};

fn params() -> McpStdioParams {
    McpStdioParams::new(env!("CARGO_BIN_EXE_mcp_fixture"))
}
fn prompt_text(result: &GetPromptResult) -> &str {
    match &result.messages[0].content {
        ContentBlock::Text(text) => &text.text,
        _ => panic!("expected text"),
    }
}
async fn catalogs(client: &McpClient) {
    let info = client.server_initialize_result();
    assert!(info.capabilities.prompts.is_some());
    assert!(info.capabilities.resources.is_some());
    let prompts = client.list_prompts().await.unwrap();
    assert_eq!(
        prompts
            .prompts
            .iter()
            .map(|prompt| prompt.name.as_str())
            .collect::<Vec<_>>(),
        ["review", "summary"]
    );
    assert_eq!(prompts.next_cursor, None);
    assert_eq!(
        serde_json::to_value(&prompts).unwrap()["_meta"],
        json!({"first":true})
    );
    for arguments in [
        None,
        Some(serde_json::from_value(json!({"language":"Rust", "custom":42})).unwrap()),
    ] {
        let prompt = client
            .get_prompt("review", arguments.clone())
            .await
            .unwrap();
        assert_eq!(prompt.description.as_deref(), Some("Generated prompt"));
        let text: Value = serde_json::from_str(prompt_text(&prompt)).unwrap();
        assert_eq!(text, json!({"name":"review","arguments":arguments}));
    }
    assert!(
        client
            .get_prompt("missing", None)
            .await
            .unwrap_err()
            .to_string()
            .contains("prompt not found")
    );
    let first = client.list_resources(None).await.unwrap();
    assert_eq!(first.next_cursor.as_deref(), Some(""));
    assert_eq!(first.resources[0].uri, "memo://fixture/note");
    let second = client.list_resources(first.next_cursor).await.unwrap();
    assert!(second.next_cursor.is_none());
    assert_eq!(
        serde_json::to_value(&second).unwrap()["_meta"],
        json!({"cursor":""})
    );
    let templates = client.list_resource_templates(None).await.unwrap();
    assert_eq!(templates.next_cursor.as_deref(), Some(""));
    assert_eq!(
        templates.resource_templates[0].uri_template,
        "memo://fixture/{name}"
    );
    let second = client
        .list_resource_templates(Some("private-cursor".into()))
        .await
        .unwrap();
    assert!(second.next_cursor.is_none());
    assert_eq!(
        serde_json::to_value(&second).unwrap()["_meta"],
        json!({"cursor":"private-cursor"})
    );
    let read = serde_json::to_value(
        client
            .read_resource("memo://requested/opaque")
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        read["contents"],
        json!([
            {"uri":"memo://requested/opaque", "mimeType":"text/plain", "text":"café"},
            {"uri":"memo://fixture/blob", "mimeType":"application/octet-stream", "blob":"AAH/"}
        ])
    );
    assert!(
        client
            .read_resource("memo://missing")
            .await
            .unwrap_err()
            .to_string()
            .contains("resource not found")
    );
}

#[tokio::test]
async fn prompts_and_resources_use_the_shared_session_on_memory_and_stdio() {
    let mut memory = InProcessToolServer::connect(
        "catalogs",
        support::Fixture::new("catalogs"),
        McpClientOptions::default(),
    )
    .await
    .unwrap();
    catalogs(&memory.client()).await;
    memory.cleanup().await.unwrap();
    let stdio = McpClient::connect_stdio("stdio", params(), McpClientOptions::default())
        .await
        .unwrap();
    catalogs(&stdio).await;
    stdio.cleanup().await.unwrap();
}

#[tokio::test]
async fn stdio_server_is_object_safe_reusable_and_does_not_retarget_old_tools() {
    let server = McpServerStdio::new(params());
    assert!(!server.is_connected().await);
    let server: &dyn McpServer = &server;
    assert!(server.name().starts_with("stdio: "));
    assert!(server.cached_tools().is_none());
    assert!(
        server
            .list_tools()
            .await
            .unwrap_err()
            .to_string()
            .contains("connect() first")
    );
    assert!(server.list_prompts().await.is_err());
    assert!(server.get_prompt("review", None).await.is_err());
    assert!(server.list_resources(None).await.is_err());
    assert!(server.list_resource_templates(None).await.is_err());
    assert!(server.read_resource("memo://test").await.is_err());
    server.cleanup().await.unwrap();
    server.connect().await.unwrap();
    server.connect().await.unwrap();
    assert_eq!(server.list_tools().await.unwrap().len(), 2);
    assert_eq!(server.list_prompts().await.unwrap().prompts.len(), 2);
    assert!(server.get_prompt("review", None).await.is_ok());
    assert!(server.list_resources(None).await.is_ok());
    assert!(server.list_resource_templates(None).await.is_ok());
    assert!(server.read_resource("memo://test").await.is_ok());
    assert!(server.call_tool("argument_state", None, None).await.is_ok());
    server.cleanup().await.unwrap();
    server.cleanup().await.unwrap();
    assert!(server.list_tools().await.is_err());
    server.connect().await.unwrap();
    assert_eq!(server.list_tools().await.unwrap().len(), 2);
    server.cleanup().await.unwrap();
}

#[tokio::test]
async fn reconnect_keeps_discovered_tools_bound_to_the_closed_session() {
    use ra_mcp::{
        filter::ToolFilter,
        tool::{McpDiscoveryOptions, discover_tools},
    };
    let server = McpServerStdio::new(params()).with_name("frozen");
    server.connect().await.unwrap();
    let original = server.client().unwrap();
    let tools = discover_tools(
        &[(original.clone(), ToolFilter::default())],
        &McpDiscoveryOptions::new(),
    )
    .await
    .unwrap();
    server.cleanup().await.unwrap();
    assert!(!original.is_connected().await);
    server.connect().await.unwrap();
    assert!(tools[0].invoke(json!({})).await.is_err());
    assert!(
        server
            .client()
            .unwrap()
            .call_tool("search", json!({}), None)
            .await
            .is_ok()
    );
    server.cleanup().await.unwrap();
}

#[tokio::test]
async fn failed_and_cancelled_initialize_leave_a_disconnected_reusable_server() {
    for arguments in [vec!["--exit-early".into()], vec!["--delay-init".into()]] {
        let server = McpServerStdio::new(params().with_args(arguments)).with_options(
            McpClientOptions::default().with_session_timeout(Duration::from_millis(40)),
        );
        assert!(server.connect().await.is_err());
        assert!(!server.is_connected().await);
        server.cleanup().await.unwrap();
    }
    let server = McpServerStdio::new(params().with_args(["--delay-init".into()]));
    assert!(
        tokio::time::timeout(Duration::from_millis(40), server.connect())
            .await
            .is_err()
    );
    assert!(!server.is_connected().await);
    server.cleanup().await.unwrap();
}

#[tokio::test]
async fn failed_transport_cleanup_is_logged_and_the_server_reconnects() {
    let server = McpServerStdio::new(params().with_args(["--wire=malformed".into()]));
    server.connect().await.unwrap();
    let retained = server.client().unwrap();
    assert!(server.read_resource("memo://fixture/note").await.is_err());
    // Python clears the session in `finally` and logs non-HTTP teardown errors.
    server.cleanup().await.unwrap();
    assert!(!server.is_connected().await);
    server.cleanup().await.unwrap();
    // The retained session handle still reports the failure it observed.
    assert!(retained.cleanup().await.is_err());
    server.connect().await.unwrap();
    assert!(server.is_connected().await);
    assert_eq!(server.list_tools().await.unwrap().len(), 2);
    server.cleanup().await.unwrap();

    // A dead session discovered by connect is replaced even when its cleanup fails.
    let server = McpServerStdio::new(params().with_args(["--wire=malformed".into()]));
    server.connect().await.unwrap();
    assert!(server.read_resource("memo://fixture/note").await.is_err());
    tokio::time::timeout(Duration::from_secs(5), async {
        while server.is_connected().await {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    server.connect().await.unwrap();
    assert!(server.is_connected().await);
    server.cleanup().await.unwrap();
}

#[tokio::test]
async fn requests_never_wait_for_connect_and_cleanup_cancels_it() {
    let server = std::sync::Arc::new(
        McpServerStdio::new(params().with_args(["--delay-init".into()]))
            .with_options(McpClientOptions::default().with_session_timeout(None)),
    );
    let connecting = tokio::spawn({
        let server = server.clone();
        async move { server.connect().await }
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    let bounded = Duration::from_secs(2);
    let error = tokio::time::timeout(bounded, server.list_tools())
        .await
        .unwrap()
        .unwrap_err();
    assert!(error.to_string().contains("connect() first"), "{error}");
    assert!(server.client().is_err());
    assert!(
        !tokio::time::timeout(bounded, server.is_connected())
            .await
            .unwrap()
    );
    tokio::time::timeout(bounded, server.cleanup())
        .await
        .unwrap()
        .unwrap();
    assert!(connecting.await.unwrap().is_err());
    assert!(!server.is_connected().await);
}

async fn start_pending_connect(
    server: Arc<McpServerStdio>,
) -> tokio::task::JoinHandle<ra_core::error::Result<()>> {
    let (started, ready) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let connecting = server.connect();
        tokio::pin!(connecting);
        let first_poll =
            std::future::poll_fn(|context| Poll::Ready(connecting.as_mut().poll(context))).await;
        assert!(
            first_poll.is_pending(),
            "connect must reach initialization or its queue"
        );
        started.send(()).unwrap();
        connecting.await
    });
    ready.await.unwrap();
    task
}

#[tokio::test]
async fn cleanup_cancels_active_and_queued_connects_without_a_session_deadline() {
    let server = Arc::new(
        McpServerStdio::new(params().with_args(["--delay-init".into()]))
            .with_options(McpClientOptions::default().with_session_timeout(None)),
    );
    let first = start_pending_connect(server.clone()).await;
    let second = start_pending_connect(server.clone()).await;
    let third = start_pending_connect(server.clone()).await;
    let cleanup = tokio::time::timeout(Duration::from_secs(1), server.cleanup()).await;
    if cleanup.is_err() {
        first.abort();
        second.abort();
        third.abort();
        let _ = first.await;
        let _ = second.await;
        let _ = third.await;
        server.cleanup().await.unwrap();
        panic!("cleanup waited for a queued initialization with no session deadline");
    }
    cleanup.unwrap().unwrap();
    for task in [first, second, third] {
        assert!(
            task.await
                .unwrap()
                .unwrap_err()
                .to_string()
                .contains("cancelled by cleanup")
        );
    }
    assert!(!server.is_connected().await);
    assert!(server.client().is_err());

    // A connection requested after cleanup gets a fresh cancellation scope.
    let reconnecting = start_pending_connect(server.clone()).await;
    tokio::time::timeout(Duration::from_secs(1), server.cleanup())
        .await
        .unwrap()
        .unwrap();
    assert!(reconnecting.await.unwrap().is_err());
}

#[tokio::test]
async fn cancelled_cleanup_does_not_prevent_later_connections() {
    let server = Arc::new(
        McpServerStdio::new(params().with_args(["--delay-init".into()]))
            .with_options(McpClientOptions::default().with_session_timeout(None)),
    );
    let connecting = start_pending_connect(server.clone()).await;
    {
        let cleanup = server.cleanup();
        tokio::pin!(cleanup);
        let first_poll =
            std::future::poll_fn(|context| Poll::Ready(cleanup.as_mut().poll(context))).await;
        assert!(first_poll.is_pending());
    }
    assert!(connecting.await.unwrap().is_err());
    let reconnecting = start_pending_connect(server.clone()).await;
    tokio::time::timeout(Duration::from_secs(1), server.cleanup())
        .await
        .unwrap()
        .unwrap();
    assert!(reconnecting.await.unwrap().is_err());
}

#[tokio::test]
async fn overlapping_cleanups_keep_connections_cancelled_until_the_last_finishes() {
    let server = Arc::new(
        McpServerStdio::new(params().with_args(["--delay-init".into()]))
            .with_options(McpClientOptions::default().with_session_timeout(None)),
    );
    let connecting = start_pending_connect(server.clone()).await;
    let mut first_cleanup = Box::pin(server.cleanup());
    let first_poll =
        std::future::poll_fn(|context| Poll::Ready(first_cleanup.as_mut().poll(context))).await;
    assert!(first_poll.is_pending());
    let mut second_cleanup = Box::pin(server.cleanup());
    let second_poll =
        std::future::poll_fn(|context| Poll::Ready(second_cleanup.as_mut().poll(context))).await;
    assert!(second_poll.is_pending());
    drop(first_cleanup);
    let error = tokio::time::timeout(Duration::from_secs(1), server.connect())
        .await
        .unwrap()
        .unwrap_err();
    assert!(error.to_string().contains("cancelled by cleanup"));
    tokio::time::timeout(Duration::from_secs(1), second_cleanup)
        .await
        .unwrap()
        .unwrap();
    assert!(connecting.await.unwrap().is_err());
    let reconnecting = start_pending_connect(server.clone()).await;
    server.cleanup().await.unwrap();
    assert!(reconnecting.await.unwrap().is_err());
}

#[tokio::test]
async fn prompt_pagination_matches_python_defaults_metadata_and_page_boundaries() {
    let mut fixture = support::Fixture::new("many");
    fixture.mode = "many_prompts";
    let mut server = InProcessToolServer::connect("many", fixture, McpClientOptions::default())
        .await
        .unwrap();
    assert_eq!(
        server.client().list_prompts().await.unwrap().prompts.len(),
        101
    );
    server.cleanup().await.unwrap();
    for (limit, success) in [(1, false), (2, true)] {
        let mut server = InProcessToolServer::connect(
            "pages",
            support::Fixture::new("pages"),
            McpClientOptions::default().with_max_list_pages(NonZeroUsize::new(limit)),
        )
        .await
        .unwrap();
        let result = server.client().list_prompts().await;
        assert_eq!(result.is_ok(), success);
        server.cleanup().await.unwrap();
    }
    for (mode, expected) in [
        ("cycle", "repeated"),
        ("pages", "100 pages"),
        ("overflow", "2048 items"),
    ] {
        let mut fixture = support::Fixture::new("limits");
        fixture.mode = mode;
        let mut server = InProcessToolServer::connect(
            "limits",
            fixture,
            McpClientOptions::default().with_codex_catalog_limits(),
        )
        .await
        .unwrap();
        let error = server
            .client()
            .list_prompts()
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains(expected), "{error}");
        assert!(!error.contains("private-secret-cursor"));
        server.cleanup().await.unwrap();
    }
}

#[derive(Clone, Copy)]
enum CatalogRequest {
    Prompts,
    Prompt,
    Resources,
    Templates,
    Read,
}
const REQUESTS: [CatalogRequest; 5] = [
    CatalogRequest::Prompts,
    CatalogRequest::Prompt,
    CatalogRequest::Resources,
    CatalogRequest::Templates,
    CatalogRequest::Read,
];
async fn invoke(client: &McpClient, request: CatalogRequest) -> ra_core::error::Result<()> {
    match request {
        CatalogRequest::Prompts => {
            client.list_prompts().await?;
        }
        CatalogRequest::Prompt => {
            client.get_prompt("review", None).await?;
        }
        CatalogRequest::Resources => {
            client.list_resources(None).await?;
        }
        CatalogRequest::Templates => {
            client.list_resource_templates(None).await?;
        }
        CatalogRequest::Read => {
            client.read_resource("memo://fixture/note").await?;
        }
    }
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn every_catalog_request_obeys_session_deadlines_and_disabled_deadlines() {
    for request in REQUESTS {
        for timeout in [None, Some(Duration::ZERO), Some(Duration::from_secs(1))] {
            let mut fixture = support::Fixture::new("delay");
            fixture.mode = "delayed_catalog";
            let mut server = InProcessToolServer::connect(
                "delay",
                fixture,
                McpClientOptions::default().with_session_timeout(timeout),
            )
            .await
            .unwrap();
            let result = invoke(&server.client(), request).await;
            if timeout.is_some_and(|timeout| !timeout.is_zero()) {
                assert_eq!(result.unwrap_err().code(), "tool.timeout");
            } else {
                result.unwrap();
            }
            server.cleanup().await.unwrap();
        }
    }
}

#[tokio::test]
async fn every_catalog_request_propagates_server_errors_and_outer_cancellation() {
    for request in REQUESTS {
        let mut fixture = support::Fixture::new("errors");
        fixture.mode = "catalog_errors";
        let mut server =
            InProcessToolServer::connect("errors", fixture, McpClientOptions::default())
                .await
                .unwrap();
        let error = invoke(&server.client(), request).await.unwrap_err();
        assert_eq!(error.code(), "tool.invalid_input");
        assert!(error.to_string().contains("catalog request rejected"));
        server.cleanup().await.unwrap();

        let mut fixture = support::Fixture::new("cancel");
        fixture.mode = "slow_catalog";
        let cancelled = fixture.cancelled.clone();
        let mut server = InProcessToolServer::connect(
            "cancel",
            fixture,
            McpClientOptions::default().with_session_timeout(None),
        )
        .await
        .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(30), invoke(&server.client(), request))
                .await
                .is_err()
        );
        tokio::time::timeout(Duration::from_secs(2), cancelled.notified())
            .await
            .unwrap();
        server.cleanup().await.unwrap();
    }
}

#[tokio::test(start_paused = true)]
async fn prompt_catalog_deadline_bounds_the_entire_listing_independently_of_session_timeout() {
    let mut fixture = support::Fixture::new("deadline");
    fixture.mode = "delayed_catalog";
    let mut server = InProcessToolServer::connect(
        "deadline",
        fixture,
        McpClientOptions::default()
            .with_session_timeout(None)
            .with_catalog_timeout(Duration::from_secs(7)),
    )
    .await
    .unwrap();
    assert_eq!(
        server.client().list_prompts().await.unwrap_err().code(),
        "tool.timeout"
    );
    server.cleanup().await.unwrap();
}

#[tokio::test]
async fn stdio_transcodes_non_utf8_in_both_directions() {
    for (encoding, wire, name) in [
        ("latin-1", "latin1", "café"),
        ("utf-16le", "utf16le", "review 💡"),
        ("utf-16be", "utf16be", "review 💡"),
        ("utf-16", "utf16", "review 💡"),
        // Python aliases resolve to Python's codecs, not to WHATWG's Windows-1252 labels.
        ("l1", "latin1", "caf\u{85}"),
        ("ISO8859-1", "latin1", "caf\u{85}"),
        ("cp819", "latin1", "caf\u{85}"),
        ("u16", "utf16", "review 💡"),
        ("UTF_16_LE", "utf16le", "review 💡"),
    ] {
        let client = McpClient::connect_stdio(
            "encoded",
            params()
                .with_args([format!("--wire={wire}")])
                .with_encoding(encoding),
            McpClientOptions::default(),
        )
        .await
        .unwrap();
        let result = client.get_prompt(name, None).await.unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(prompt_text(&result)).unwrap()["name"],
            name
        );
        let read = serde_json::to_value(client.read_resource("memo://fixture/note").await.unwrap())
            .unwrap();
        assert_eq!(read["contents"][0]["text"], "café");
        client.cleanup().await.unwrap();
    }
}

#[tokio::test]
async fn stdio_text_error_policies_match_strict_ignore_and_replace() {
    for (handler, expected) in [
        (McpEncodingErrorHandler::Ignore, "caf"),
        (McpEncodingErrorHandler::Replace, "caf�"),
    ] {
        let client = McpClient::connect_stdio(
            "malformed",
            params()
                .with_args(["--wire=malformed".into()])
                .with_encoding_error_handler(handler),
            McpClientOptions::default(),
        )
        .await
        .unwrap();
        let read = serde_json::to_value(client.read_resource("memo://fixture/note").await.unwrap())
            .unwrap();
        assert_eq!(read["contents"][0]["text"], expected);
        client.cleanup().await.unwrap();
    }
    for (encoding, handler, expected) in [
        ("ascii", McpEncodingErrorHandler::Ignore, "review_"),
        ("ascii", McpEncodingErrorHandler::Replace, "review_?"),
        ("646", McpEncodingErrorHandler::Replace, "review_?"),
        (
            "ANSI_X3.4-1968",
            McpEncodingErrorHandler::Replace,
            "review_?",
        ),
    ] {
        let client = McpClient::connect_stdio(
            "ascii",
            params()
                .with_encoding(encoding)
                .with_encoding_error_handler(handler),
            McpClientOptions::default(),
        )
        .await
        .unwrap();
        let result = client.get_prompt("review_猫", None).await.unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(prompt_text(&result)).unwrap()["name"],
            expected
        );
        client.cleanup().await.unwrap();
    }
    let client = McpClient::connect_stdio(
        "strict",
        params().with_encoding("ascii"),
        McpClientOptions::default(),
    )
    .await
    .unwrap();
    assert!(client.get_prompt("review_猫", None).await.is_err());
    // A codec worker failure is reported during shutdown rather than claiming successful cleanup.
    assert!(client.cleanup().await.is_err());
}

#[tokio::test]
async fn invalid_launch_configuration_fails_without_retaining_sensitive_values() {
    for (name, params) in [
        ("valid", params().with_encoding("unknown-secret-codec")),
        ("bad\nname", params()),
        (
            "valid",
            McpStdioParams::new("missing-executable-private-token"),
        ),
    ] {
        let error = McpClient::connect_stdio(name, params, McpClientOptions::default())
            .await
            .err()
            .unwrap();
        let text = format!("{error:?} {error}");
        assert!(!text.contains("secret-codec"));
        assert!(!text.contains("private-token"));
        assert!(!text.contains("bad\nname"));
    }
}

#[tokio::test]
async fn utf8_strict_rejects_malformed_input_even_without_a_request_deadline() {
    let client = McpClient::connect_stdio(
        "strict",
        params().with_args(["--wire=malformed".into()]),
        McpClientOptions::default().with_session_timeout(None),
    )
    .await
    .unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        client.read_resource("memo://fixture/note"),
    )
    .await
    .unwrap();
    assert!(result.is_err());
    assert!(client.cleanup().await.is_err());
    assert!(client.cleanup().await.is_err());
}

#[tokio::test]
async fn text_transcoding_preserves_multibyte_characters_across_buffer_boundaries() {
    let name = "é💡".repeat(5000);
    for (encoding, arguments) in [
        ("utf-8", vec![]),
        ("utf-16le", vec!["--wire=utf16le".into()]),
    ] {
        let client = McpClient::connect_stdio(
            "fragmented",
            params().with_args(arguments).with_encoding(encoding),
            McpClientOptions::default(),
        )
        .await
        .unwrap();
        let result = client.get_prompt(&name, None).await.unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(prompt_text(&result)).unwrap()["name"],
            name
        );
        client.cleanup().await.unwrap();
    }
}

#[cfg(unix)]
#[tokio::test]
async fn stdio_cleanup_waits_until_the_owned_child_is_reaped() {
    for arguments in [vec![], vec!["--wire=latin1".into()]] {
        let encoding = if arguments.is_empty() {
            "utf-8"
        } else {
            "latin-1"
        };
        let client = McpClient::connect_stdio(
            "process",
            params().with_args(arguments).with_encoding(encoding),
            McpClientOptions::default(),
        )
        .await
        .unwrap();
        let result = client.call_tool("env", None, None).await.unwrap();
        let ContentBlock::Text(text) = &result.content[0] else {
            panic!("expected text");
        };
        let pid = serde_json::from_str::<Value>(&text.text).unwrap()["pid"]
            .as_u64()
            .unwrap();
        let pid = rustix::process::Pid::from_raw(i32::try_from(pid).unwrap()).unwrap();
        rustix::process::test_kill_process(pid).unwrap();
        client.cleanup().await.unwrap();
        assert_eq!(
            rustix::process::test_kill_process(pid),
            Err(rustix::io::Errno::SRCH)
        );
    }
}
