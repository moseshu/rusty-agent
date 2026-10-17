//! Initialization preserves protocol errors without retaining transport or diagnostic payloads.
#[path = "../support/mod.rs"]
mod support;

use ra_core::error::Error;
use ra_mcp::{
    McpServer,
    client::{
        McpClientOptions,
        stdio::{McpServerStdio, McpStdioParams},
    },
    in_process::InProcessToolServer,
};
use rmcp::{ErrorData, model::ErrorCode};
use serde_json::json;

fn assert_protocol_error(error: &Error, code: i32, message: &str, classification: &str) {
    assert_eq!(error.code(), classification);
    let rendered = format!("{error} {error:?}");
    assert!(rendered.contains(&code.to_string()), "{rendered}");
    assert!(rendered.contains(message), "{rendered}");
    assert!(!rendered.contains("initialize-data-credential"));
    assert!(std::error::Error::source(error).is_none());
}

#[tokio::test]
async fn initialize_protocol_error_keeps_code_message_and_classification_in_memory() {
    for (code, classification) in [
        (ErrorCode::INVALID_PARAMS, "tool.invalid_input"),
        (ErrorCode::INTERNAL_ERROR, "tool.execution_failed"),
        (ErrorCode(-32001), "tool.execution_failed"),
    ] {
        let error = InProcessToolServer::connect(
            "rejected",
            support::RejectInitialize(ErrorData::new(
                code,
                "workspace configuration is unsupported",
                Some(json!({"token": "initialize-data-credential"})),
            )),
            McpClientOptions::default(),
        )
        .await
        .err()
        .expect("the server must reject initialize");
        assert_protocol_error(
            &error,
            code.0,
            "workspace configuration is unsupported",
            classification,
        );
    }
}

#[tokio::test]
async fn initialize_protocol_error_keeps_details_and_cleans_up_stdio() {
    let server = McpServerStdio::new(
        McpStdioParams::new(env!("CARGO_BIN_EXE_mcp_fixture")).with_args(["--reject-init".into()]),
    );
    for _ in 0..2 {
        let error = server.connect().await.unwrap_err();
        assert_protocol_error(
            &error,
            -32602,
            "workspace configuration is unsupported",
            "tool.invalid_input",
        );
        assert!(!server.is_connected().await);
        assert!(server.client().is_err());
        server.cleanup().await.unwrap();
    }
}
