//! The server's requests to the client while a turn runs, as Codex's app-server has them
//! (`app-server-protocol/src/protocol/common.rs`, `protocol/v2/{item,permissions}.rs`,
//! `app-server/src/outgoing_message.rs`, `app-server/src/transport.rs`,
//! `app-server/src/bespoke_event_handling.rs`, `app-server/src/dynamic_tools.rs`).
//!
//! The wire cases port Codex's serialization tests of the five requests and their answers, and pin
//! each payload against the shape of Codex's generated schema. The pipeline cases run the server
//! in this process with connections whose write queues the test holds; turns do not run in the
//! server yet, so a case raises requests for a thread through the crate's `test-api` door, as a
//! turn will. The answer cases port what Codex's handlers make of each answer.
//!
//! A message the server must not send is checked by sending another after it: the router writes a
//! connection's messages in the order they are sent, so if the next one to arrive is the later
//! message, the earlier one was not written.

use std::{num::NonZeroUsize, time::Duration};

use ra_core::session::SessionId;
use ra_protocol::{
    frame::{
        JsonRpcError, JsonRpcErrorObject, JsonRpcMessage, JsonRpcRequest, JsonRpcResponse,
        RequestId, RequestResult,
    },
    server::{
        AppServer,
        protocol::{
            AdditionalFileSystemPermissions, AdditionalNetworkPermissions,
            AdditionalPermissionProfile, ClientInfo, CommandAction,
            CommandExecutionApprovalDecision, CommandExecutionApprovalKind,
            CommandExecutionRequestApprovalParams, CommandExecutionRequestApprovalResponse,
            DynamicToolCallOutputContentItem, DynamicToolCallParams, DynamicToolCallResponse,
            ExecPolicyAmendment, FileChangeApprovalDecision, FileChangeRequestApprovalParams,
            FileChangeRequestApprovalResponse, FileSystemAccessMode, FileSystemPath,
            FileSystemSandboxEntry, FileSystemSpecialPath, GrantedPermissionProfile,
            InitializeCapabilities, InitializeParams, NetworkApprovalContext,
            NetworkApprovalProtocol, NetworkPolicyAmendment, NetworkPolicyRuleAction,
            PermissionGrantScope, PermissionsRequestApprovalParams,
            PermissionsRequestApprovalResponse, RequestPermissionProfile,
            SERVER_REQUEST_RESOLVED_METHOD, ServerRequest, ServerRequestPayload,
            ServerRequestResolvedNotification, ToolRequestUserInputAnswer,
            ToolRequestUserInputOption, ToolRequestUserInputParams, ToolRequestUserInputQuestion,
            ToolRequestUserInputResponse,
        },
        test_api::{
            ClientAnswer, ReviewDecision, ServerNotifier, ThreadRequester,
            command_execution_approval_decision, dynamic_tool_call_response,
            file_change_approval_decision, request_permissions_response,
            request_user_input_response,
        },
    },
    transport::{
        CHANNEL_CAPACITY, ConnectionId, ConnectionOrigin, QueuedOutgoingMessage, TransportEvent,
        test_api::connection_opened,
    },
};
use serde_json::{Value, json};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
    time::timeout,
};
use tokio_util::sync::CancellationToken;

/// How long a case waits for any one message or answer.
const PATIENCE: Duration = Duration::from_secs(10);

const THREAD: &str = "thread-1";

// ---------------------------------------------------------------------------------------------
// The server in this process
// ---------------------------------------------------------------------------------------------

/// A running server and the channel its transports would report on.
struct Server {
    events: mpsc::Sender<TransportEvent>,
    notifier: ServerNotifier,
    _task: JoinHandle<()>,
}

impl Server {
    /// Starts a server, first making what `before_run` makes of it — the requesters a case uses,
    /// which must be made before the server runs.
    fn start<R>(before_run: impl FnOnce(&AppServer) -> R) -> (Self, R) {
        let server =
            AppServer::new(std::env::temp_dir().join("rusty-agent-server-request-test")).unwrap();
        let notifier = ServerNotifier::new(&server);
        let made = before_run(&server);
        let (events, transport_event_rx) = mpsc::channel(CHANNEL_CAPACITY);
        let task = tokio::spawn(server.run(transport_event_rx));
        (
            Self {
                events,
                notifier,
                _task: task,
            },
            made,
        )
    }

    /// Opens a WebSocket connection with the id `id`.
    async fn open(&self, id: u64) -> Connection {
        let id = ConnectionId::new(id);
        let (writer, written) = mpsc::channel(CHANNEL_CAPACITY);
        self.events
            .send(connection_opened(
                id,
                ConnectionOrigin::WebSocket,
                writer,
                Some(CancellationToken::new()),
            ))
            .await
            .unwrap();
        Connection { id, written }
    }

    async fn send(&self, connection: &Connection, message: impl Into<JsonRpcMessage>) {
        self.events
            .send(TransportEvent::IncomingMessage {
                connection_id: connection.id,
                message: message.into(),
            })
            .await
            .unwrap();
    }

    /// Initializes `connection`, opting into the experimental API if `experimental`.
    ///
    /// The router learns of the handshake once the server has handled it, which may be after the
    /// answer has been written; asking again waits for that, as `Already initialized` arrives once
    /// it has.
    async fn initialize(&self, connection: &mut Connection, experimental: bool) {
        let params = InitializeParams::new(ClientInfo::new("client", "0.1.0"))
            .with_capabilities(InitializeCapabilities::new().with_experimental_api(experimental));
        for _ in 0..2 {
            self.send(
                connection,
                JsonRpcRequest::new(0, "initialize")
                    .with_params(serde_json::to_value(&params).unwrap()),
            )
            .await;
            connection.next().await;
        }
    }

    /// Sends every initialized connection a notification of `method`, which marks where the
    /// messages sent before it end.
    async fn mark(&self, method: &str) {
        self.notifier.send(&[], method, json!({}), None).await;
    }
}

/// A connection as the server sees it, and what the server writes to it.
struct Connection {
    id: ConnectionId,
    written: mpsc::Receiver<QueuedOutgoingMessage>,
}

impl Connection {
    /// The next message the server writes, as JSON. A notification's `emittedAtMs` says when it
    /// was sent; it is checked and taken off, so the cases compare what the notification says.
    async fn next(&mut self) -> Value {
        let queued = timeout(PATIENCE, self.written.recv())
            .await
            .expect("the server writes a message")
            .expect("the server keeps the write queue");
        let mut message = serde_json::to_value(queued.message()).unwrap();
        if message.get("method").is_some() && message.get("id").is_none() {
            let emitted_at_ms = message
                .as_object_mut()
                .and_then(|message| message.remove("emittedAtMs"))
                .and_then(|emitted_at_ms| emitted_at_ms.as_i64());
            assert!(
                emitted_at_ms.is_some_and(|emitted_at_ms| emitted_at_ms > 0),
                "{message}"
            );
        }
        message
    }

    /// Asserts the next message is the marker notification `method`.
    async fn expect_mark(&mut self, method: &str) {
        let message = self.next().await;
        assert_eq!(message["method"], method, "{message}");
    }
}

async fn answer_of(answer: oneshot::Receiver<RequestResult>) -> ClientAnswer {
    timeout(PATIENCE, answer).await.expect("the answer arrives")
}

fn thread() -> SessionId {
    SessionId::new(THREAD)
}

fn resolved(request_id: i64) -> Value {
    json!({
        "method": SERVER_REQUEST_RESOLVED_METHOD,
        "params": {"threadId": THREAD, "requestId": request_id},
    })
}

fn command_approval() -> CommandExecutionRequestApprovalParams {
    CommandExecutionRequestApprovalParams::new(THREAD, "turn-1", "call-1", 1_700_000_000_000)
        .with_command("cat file")
        .with_cwd("/tmp")
}

fn file_change_approval(item_id: &str) -> FileChangeRequestApprovalParams {
    FileChangeRequestApprovalParams::new(THREAD, "turn-1", item_id, 0)
}

fn dynamic_tool_call(call_id: &str) -> DynamicToolCallParams {
    DynamicToolCallParams::new(THREAD, "turn-1", call_id, "tool", json!({}))
}

fn user_input(item_id: &str) -> ToolRequestUserInputParams {
    ToolRequestUserInputParams::new(THREAD, "turn-1", item_id, [])
}

// ---------------------------------------------------------------------------------------------
// The wire
// ---------------------------------------------------------------------------------------------

/// Adapted from Codex's `serialize_server_request`, with the v2 command approval in place of the
/// deprecated v1 one: the request is written by method with its id and parameters, built from
/// its payload, and read back.
#[test]
fn serialize_server_request() {
    let params = CommandExecutionRequestApprovalParams::new("thr_123", "turn_123", "call_42", 7)
        .with_approval_id("approval-42")
        .with_command("echo hello")
        .with_cwd("/tmp")
        .with_reason("because tests")
        .with_command_actions([CommandAction::unknown("echo hello")]);
    let request = ServerRequest::CommandExecutionRequestApproval {
        request_id: RequestId::Integer(7),
        params: params.clone(),
    };
    let expected = json!({
        "method": "item/commandExecution/requestApproval",
        "id": 7,
        "params": {
            "kind": "command",
            "threadId": "thr_123",
            "turnId": "turn_123",
            "itemId": "call_42",
            "startedAtMs": 7,
            "approvalId": "approval-42",
            "environmentId": null,
            "reason": "because tests",
            "command": "echo hello",
            "cwd": "/tmp",
            "commandActions": [{"type": "unknown", "command": "echo hello"}],
        },
    });
    assert_eq!(serde_json::to_value(&request).unwrap(), expected);

    let payload = ServerRequestPayload::CommandExecutionRequestApproval(params);
    assert_eq!(request.id(), &RequestId::Integer(7));
    assert_eq!(
        request.method_name(),
        "item/commandExecution/requestApproval"
    );
    assert_eq!(payload.request_with_id(RequestId::Integer(7)), request);

    let rpc = JsonRpcRequest::try_from(request.clone()).unwrap();
    assert_eq!(rpc.method(), "item/commandExecution/requestApproval");
    assert_eq!(rpc.params(), Some(&expected["params"]));
    assert_eq!(ServerRequest::try_from(rpc).unwrap(), request);
}

/// Each request's parameters have the members of Codex's generated schema: optional members of a
/// command approval are left out when unset, `environmentId` is always written, and the other
/// requests write their unset optional members as `null`.
#[test]
fn server_requests_use_codex_wire_shapes() {
    let cases = [
        (
            ServerRequestPayload::CommandExecutionRequestApproval(
                CommandExecutionRequestApprovalParams::new("t", "u", "i", 1),
            ),
            json!({
                "method": "item/commandExecution/requestApproval",
                "id": 1,
                "params": {
                    "kind": "command", "threadId": "t", "turnId": "u", "itemId": "i",
                    "startedAtMs": 1, "environmentId": null,
                },
            }),
        ),
        (
            ServerRequestPayload::FileChangeRequestApproval(FileChangeRequestApprovalParams::new(
                "t", "u", "i", 2,
            )),
            json!({
                "method": "item/fileChange/requestApproval",
                "id": 1,
                "params": {
                    "threadId": "t", "turnId": "u", "itemId": "i", "startedAtMs": 2,
                    "reason": null, "grantRoot": null,
                },
            }),
        ),
        (
            ServerRequestPayload::ToolRequestUserInput(ToolRequestUserInputParams::new(
                "t",
                "u",
                "i",
                [ToolRequestUserInputQuestion::new(
                    "q1",
                    "Confirm",
                    "Continue?",
                )],
            )),
            json!({
                "method": "item/tool/requestUserInput",
                "id": 1,
                "params": {
                    "threadId": "t", "turnId": "u", "itemId": "i",
                    "questions": [{
                        "id": "q1", "header": "Confirm", "question": "Continue?",
                        "isOther": false, "isSecret": false, "options": null,
                    }],
                    "isBlocking": true, "autoResolutionMs": null,
                },
            }),
        ),
        (
            ServerRequestPayload::PermissionsRequestApproval(
                PermissionsRequestApprovalParams::new(
                    "t",
                    "u",
                    "i",
                    3,
                    "/repo",
                    RequestPermissionProfile::new(),
                ),
            ),
            json!({
                "method": "item/permissions/requestApproval",
                "id": 1,
                "params": {
                    "threadId": "t", "turnId": "u", "itemId": "i", "environmentId": null,
                    "startedAtMs": 3, "cwd": "/repo", "reason": null,
                    "permissions": {"network": null, "fileSystem": null},
                },
            }),
        ),
        (
            ServerRequestPayload::DynamicToolCall(
                DynamicToolCallParams::new("t", "u", "c", "lookup", json!({"q": 1}))
                    .with_namespace("ns"),
            ),
            json!({
                "method": "item/tool/call",
                "id": 1,
                "params": {
                    "threadId": "t", "turnId": "u", "callId": "c", "namespace": "ns",
                    "tool": "lookup", "arguments": {"q": 1},
                },
            }),
        ),
    ];
    for (payload, expected) in cases {
        let request = payload.request_with_id(RequestId::Integer(1));
        assert_eq!(serde_json::to_value(&request).unwrap(), expected);
        let read: ServerRequest = serde_json::from_value(expected).unwrap();
        assert_eq!(read, request);
    }
}

/// Every member of a command approval, written as Codex writes it.
#[test]
fn command_execution_request_approval_writes_every_member() {
    let params = command_approval()
        .with_kind(CommandExecutionApprovalKind::WriteStdin)
        .with_environment_id("remote")
        .with_network_approval_context(NetworkApprovalContext::new(
            "example.com",
            NetworkApprovalProtocol::Socks5Tcp,
        ))
        .with_command_actions([
            CommandAction::read("cat file", "file", "/tmp/file"),
            CommandAction::list_files("ls", None),
            CommandAction::search("rg x", Some("x".to_owned()), Some("src".to_owned())),
        ])
        .with_additional_permissions(
            AdditionalPermissionProfile::new()
                .with_network(AdditionalNetworkPermissions::new().with_enabled(true)),
        )
        .with_proposed_execpolicy_amendment(ExecPolicyAmendment::new(["cat"]))
        .with_proposed_network_policy_amendments([NetworkPolicyAmendment::new(
            "example.com",
            NetworkPolicyRuleAction::Allow,
        )])
        .with_available_decisions([
            CommandExecutionApprovalDecision::Accept,
            CommandExecutionApprovalDecision::Cancel,
        ]);
    let value = serde_json::to_value(&params).unwrap();
    assert_eq!(
        value,
        json!({
            "kind": "writeStdin",
            "threadId": THREAD,
            "turnId": "turn-1",
            "itemId": "call-1",
            "startedAtMs": 1_700_000_000_000_i64,
            "environmentId": "remote",
            "networkApprovalContext": {"host": "example.com", "protocol": "socks5Tcp"},
            "command": "cat file",
            "cwd": "/tmp",
            "commandActions": [
                {"type": "read", "command": "cat file", "name": "file", "path": "/tmp/file"},
                {"type": "listFiles", "command": "ls", "path": null},
                {"type": "search", "command": "rg x", "query": "x", "path": "src"},
            ],
            "additionalPermissions": {"network": {"enabled": true}, "fileSystem": null},
            "proposedExecpolicyAmendment": ["cat"],
            "proposedNetworkPolicyAmendments": [{"host": "example.com", "action": "allow"}],
            "availableDecisions": ["accept", "cancel"],
        })
    );
    let read: CommandExecutionRequestApprovalParams = serde_json::from_value(value).unwrap();
    assert_eq!(read, params);
}

/// A command approval from a server older than `kind` reads as one for a command.
#[test]
fn command_execution_request_approval_kind_defaults_to_command() {
    let params: CommandExecutionRequestApprovalParams = serde_json::from_value(json!({
        "threadId": "t", "turnId": "u", "itemId": "i", "startedAtMs": 0,
    }))
    .unwrap();
    assert_eq!(params.kind(), CommandExecutionApprovalKind::Command);
    assert_eq!(params.environment_id(), None);
}

/// The answers to the two approvals are written as Codex writes them: unit decisions as strings,
/// the amendments as objects whose members keep their `snake_case` names.
#[test]
fn approval_decisions_use_codex_wire_shapes() {
    let cases = [
        (CommandExecutionApprovalDecision::Accept, json!("accept")),
        (
            CommandExecutionApprovalDecision::AcceptForSession,
            json!("acceptForSession"),
        ),
        (
            CommandExecutionApprovalDecision::accept_with_execpolicy_amendment(
                ExecPolicyAmendment::new(["git", "status"]),
            ),
            json!({"acceptWithExecpolicyAmendment": {"execpolicy_amendment": ["git", "status"]}}),
        ),
        (
            CommandExecutionApprovalDecision::apply_network_policy_amendment(
                NetworkPolicyAmendment::new("example.com", NetworkPolicyRuleAction::Deny),
            ),
            json!({"applyNetworkPolicyAmendment": {
                "network_policy_amendment": {"host": "example.com", "action": "deny"},
            }}),
        ),
        (CommandExecutionApprovalDecision::Decline, json!("decline")),
        (CommandExecutionApprovalDecision::Cancel, json!("cancel")),
    ];
    for (decision, expected) in cases {
        let response = CommandExecutionRequestApprovalResponse::new(decision);
        let written = serde_json::to_value(&response).unwrap();
        assert_eq!(written, json!({"decision": expected}));
        let read: CommandExecutionRequestApprovalResponse =
            serde_json::from_value(written).unwrap();
        assert_eq!(read, response);
    }

    let cases = [
        (FileChangeApprovalDecision::Accept, "accept"),
        (
            FileChangeApprovalDecision::AcceptForSession,
            "acceptForSession",
        ),
        (FileChangeApprovalDecision::Decline, "decline"),
        (FileChangeApprovalDecision::Cancel, "cancel"),
    ];
    for (decision, expected) in cases {
        let response = FileChangeRequestApprovalResponse::new(decision);
        assert_eq!(
            serde_json::to_value(response).unwrap(),
            json!({"decision": expected})
        );
    }
}

/// Codex's `command_execution_request_approval_additional_permissions_is_marked_experimental`,
/// with the second experimental field: the first one set names the reason.
#[test]
fn command_execution_request_approval_additional_permissions_is_marked_experimental() {
    assert_eq!(command_approval().experimental_reason(), None);
    let additional = AdditionalPermissionProfile::new()
        .with_file_system(AdditionalFileSystemPermissions::new().with_read(["/tmp/allowed"]));
    assert_eq!(
        command_approval()
            .with_additional_permissions(additional.clone())
            .with_available_decisions([CommandExecutionApprovalDecision::Accept])
            .experimental_reason(),
        Some("item/commandExecution/requestApproval.additionalPermissions")
    );
    assert_eq!(
        command_approval()
            .with_available_decisions([CommandExecutionApprovalDecision::Accept])
            .experimental_reason(),
        Some("item/commandExecution/requestApproval.availableDecisions")
    );
}

/// Codex's `permissions_request_approval_uses_request_permission_profile`, without the conversion
/// into its sandbox's paths.
#[test]
fn permissions_request_approval_uses_request_permission_profile() {
    let params = serde_json::from_value::<PermissionsRequestApprovalParams>(json!({
        "threadId": "thr_123",
        "turnId": "turn_123",
        "itemId": "call_123",
        "environmentId": "remote",
        "startedAtMs": 1,
        "cwd": "/repo",
        "reason": "Select a workspace root",
        "permissions": {
            "network": {"enabled": true},
            "fileSystem": {"read": ["/tmp/read-only"], "write": ["/tmp/read-write"]},
        },
    }))
    .expect("permissions request should deserialize");

    assert_eq!(params.cwd(), "/repo");
    assert_eq!(params.environment_id(), Some("remote"));
    assert_eq!(params.reason(), Some("Select a workspace root"));
    assert_eq!(
        params.permissions(),
        &RequestPermissionProfile::new()
            .with_network(AdditionalNetworkPermissions::new().with_enabled(true))
            .with_file_system(
                AdditionalFileSystemPermissions::new()
                    .with_read(["/tmp/read-only"])
                    .with_write(["/tmp/read-write"])
            )
    );
}

/// Codex's `permissions_request_approval_rejects_macos_permissions`: a kind of permission the
/// profile cannot express is refused, not dropped.
#[test]
fn permissions_request_approval_rejects_macos_permissions() {
    let error = serde_json::from_value::<PermissionsRequestApprovalParams>(json!({
        "threadId": "thr_123",
        "turnId": "turn_123",
        "itemId": "call_123",
        "startedAtMs": 1,
        "cwd": "/repo",
        "reason": "Select a workspace root",
        "permissions": {
            "network": null,
            "fileSystem": null,
            "macos": {"preferences": "read_only"},
        },
    }))
    .expect_err("permissions request should reject macos permissions");
    assert!(
        error.to_string().contains("unknown field `macos`"),
        "unexpected error: {error}"
    );
}

/// Codex's `additional_file_system_permissions_rejects_zero_glob_scan_depth`.
#[test]
fn additional_file_system_permissions_rejects_zero_glob_scan_depth() {
    serde_json::from_value::<AdditionalFileSystemPermissions>(json!({
        "read": null,
        "write": null,
        "globScanMaxDepth": 0,
        "entries": [],
    }))
    .expect_err("zero glob scan depth should fail deserialization");
}

/// Codex's `legacy_current_working_directory_special_path_deserializes_as_project_roots`.
#[test]
fn legacy_current_working_directory_special_path_deserializes_as_project_roots() {
    let special_path = serde_json::from_value::<FileSystemSpecialPath>(json!({
        "kind": "current_working_directory",
    }))
    .expect("legacy cwd special path should deserialize");
    assert_eq!(special_path, FileSystemSpecialPath::project_roots(None));
    assert_eq!(
        serde_json::to_value(&special_path).unwrap(),
        json!({"kind": "project_roots", "subpath": null})
    );
}

/// The entries form of filesystem permissions, written as Codex's schema has it.
#[test]
fn file_system_entries_use_codex_wire_shapes() {
    let permissions = AdditionalFileSystemPermissions::new()
        .with_glob_scan_max_depth(NonZeroUsize::new(3).unwrap())
        .with_entries([
            FileSystemSandboxEntry::new(FileSystemPath::path("/repo"), FileSystemAccessMode::Write),
            FileSystemSandboxEntry::new(
                FileSystemPath::glob_pattern("**/*.env"),
                FileSystemAccessMode::Deny,
            ),
            FileSystemSandboxEntry::new(
                FileSystemPath::special(FileSystemSpecialPath::Tmpdir),
                FileSystemAccessMode::Read,
            ),
            FileSystemSandboxEntry::new(
                FileSystemPath::special(FileSystemSpecialPath::unknown(
                    "home",
                    Some(".cache".to_owned()),
                )),
                FileSystemAccessMode::Read,
            ),
        ]);
    let value = serde_json::to_value(&permissions).unwrap();
    assert_eq!(
        value,
        json!({
            "read": null,
            "write": null,
            "globScanMaxDepth": 3,
            "entries": [
                {"path": {"type": "path", "path": "/repo"}, "access": "write"},
                {"path": {"type": "glob_pattern", "pattern": "**/*.env"}, "access": "deny"},
                {"path": {"type": "special", "value": {"kind": "tmpdir"}}, "access": "read"},
                {
                    "path": {
                        "type": "special",
                        "value": {"kind": "unknown", "path": "home", "subpath": ".cache"},
                    },
                    "access": "read",
                },
            ],
        })
    );
    assert_eq!(
        serde_json::from_value::<AdditionalFileSystemPermissions>(value).unwrap(),
        permissions
    );
}

/// Codex's `permissions_request_approval_response_uses_granted_permission_profile_without_macos`,
/// without the conversion into its sandbox's paths, and its two scope cases.
#[test]
fn permissions_request_approval_response_reads_grants_scope_and_strict_review() {
    let response = serde_json::from_value::<PermissionsRequestApprovalResponse>(json!({
        "permissions": {
            "network": {"enabled": true},
            "fileSystem": {"read": ["/tmp/read-only"], "write": ["/tmp/read-write"]},
        },
    }))
    .expect("permissions response should deserialize");
    assert_eq!(
        response.permissions(),
        &GrantedPermissionProfile::new()
            .with_network(AdditionalNetworkPermissions::new().with_enabled(true))
            .with_file_system(
                AdditionalFileSystemPermissions::new()
                    .with_read(["/tmp/read-only"])
                    .with_write(["/tmp/read-write"])
            )
    );

    // permissions_request_approval_response_defaults_scope_to_turn
    let response = serde_json::from_value::<PermissionsRequestApprovalResponse>(json!({
        "permissions": {},
    }))
    .unwrap();
    assert_eq!(response.scope(), PermissionGrantScope::Turn);
    assert_eq!(response.strict_auto_review(), None);
    assert_eq!(
        serde_json::to_value(&response).unwrap(),
        json!({"permissions": {}, "scope": "turn"})
    );

    // permissions_request_approval_response_accepts_strict_auto_review
    let response = serde_json::from_value::<PermissionsRequestApprovalResponse>(json!({
        "permissions": {},
        "strictAutoReview": true,
        "scope": "session",
    }))
    .unwrap();
    assert_eq!(response.strict_auto_review(), Some(true));
    assert_eq!(response.scope(), PermissionGrantScope::Session);
}

/// Codex's `dynamic_tool_response_serializes_content_items` and
/// `dynamic_tool_response_serializes_text_image_and_audio_content_items`.
#[test]
fn dynamic_tool_response_serializes_text_image_and_audio_content_items() {
    let response = DynamicToolCallResponse::new(
        [
            DynamicToolCallOutputContentItem::input_text("dynamic-ok"),
            DynamicToolCallOutputContentItem::input_image("data:image/png;base64,AAA"),
            DynamicToolCallOutputContentItem::input_audio("data:audio/wav;base64,YXVkaW8="),
        ],
        true,
    );
    let value = serde_json::to_value(&response).unwrap();
    assert_eq!(
        value,
        json!({
            "contentItems": [
                {"type": "inputText", "text": "dynamic-ok"},
                {"type": "inputImage", "imageUrl": "data:image/png;base64,AAA"},
                {"type": "inputAudio", "audioUrl": "data:audio/wav;base64,YXVkaW8="},
            ],
            "success": true,
        })
    );
    assert_eq!(
        serde_json::from_value::<DynamicToolCallResponse>(value).unwrap(),
        response
    );
}

/// Codex's `tool_request_user_input_params_default_legacy_missing_is_blocking_to_true`.
#[test]
fn tool_request_user_input_params_default_legacy_missing_is_blocking_to_true() {
    let params = serde_json::from_value::<ToolRequestUserInputParams>(json!({
        "threadId": "thread-1",
        "turnId": "turn-1",
        "itemId": "call-1",
        "questions": [{
            "id": "q1",
            "header": "Confirm",
            "question": "Continue?",
            "options": [{"label": "Yes", "description": "Continue."}],
        }],
        "autoResolutionMs": 60_000,
    }))
    .expect("legacy request_user_input params should deserialize");
    assert_eq!(
        params,
        ToolRequestUserInputParams::new(
            "thread-1",
            "turn-1",
            "call-1",
            [
                ToolRequestUserInputQuestion::new("q1", "Confirm", "Continue?")
                    .with_options([ToolRequestUserInputOption::new("Yes", "Continue.")])
            ],
        )
        .with_auto_resolution_ms(60_000)
    );
    assert!(params.is_blocking());

    let params = serde_json::from_value::<ToolRequestUserInputParams>(json!({
        "threadId": "t", "turnId": "u", "itemId": "i", "questions": [], "isBlocking": false,
    }))
    .unwrap();
    assert!(!params.is_blocking());
}

/// The user's answers, keyed by question id, and `serverRequest/resolved`, as Codex writes them.
#[test]
fn user_input_answers_and_resolved_notification_use_codex_wire_shapes() {
    let response = ToolRequestUserInputResponse::new()
        .with_answer("q2", ToolRequestUserInputAnswer::new(["b"]))
        .with_answer("q1", ToolRequestUserInputAnswer::new(["a", "c"]));
    let value = serde_json::to_value(&response).unwrap();
    assert_eq!(
        value,
        json!({"answers": {"q1": {"answers": ["a", "c"]}, "q2": {"answers": ["b"]}}})
    );
    assert_eq!(
        serde_json::from_value::<ToolRequestUserInputResponse>(value).unwrap(),
        response
    );

    let notification = ServerRequestResolvedNotification::new("thr", RequestId::Integer(4));
    assert_eq!(
        serde_json::to_value(&notification).unwrap(),
        json!({"threadId": "thr", "requestId": 4})
    );
}

/// A request of a method the client does not know, or with parameters that do not fit, is not
/// read, as Codex's `TryFrom<JSONRPCRequest>` refuses it.
#[test]
fn unknown_server_requests_are_not_read() {
    let unknown = JsonRpcRequest::new(1, "account/chatgptAuthTokens/refresh")
        .with_params(json!({"reason": "unauthorized"}));
    assert!(ServerRequest::try_from(unknown).is_err());
    let unfit = JsonRpcRequest::new(1, "item/tool/call").with_params(json!({"threadId": "t"}));
    assert!(ServerRequest::try_from(unfit).is_err());
}

// ---------------------------------------------------------------------------------------------
// Sending, answering and withdrawing
// ---------------------------------------------------------------------------------------------

/// Codex's `pending_requests_for_thread_returns_thread_requests_in_request_id_order`: ids are
/// integers from zero, shared by every thread, and a thread's requests are listed in their order.
#[tokio::test]
async fn pending_requests_for_thread_returns_thread_requests_in_request_id_order() {
    let (_server, (requester, other_thread)) = Server::start(|server| {
        (
            ThreadRequester::new(server, &[ConnectionId::new(1)], thread()),
            ThreadRequester::new(server, &[ConnectionId::new(1)], SessionId::new("thread-2")),
        )
    });

    let (dynamic_tool_request_id, _dynamic_tool_waiter) = requester
        .send_request(ServerRequestPayload::DynamicToolCall(dynamic_tool_call(
            "call-0",
        )))
        .await;
    let (other_request_id, _other_waiter) = other_thread
        .send_request(ServerRequestPayload::DynamicToolCall(dynamic_tool_call(
            "call-x",
        )))
        .await;
    let (first_request_id, _first_waiter) = requester
        .send_request(ServerRequestPayload::ToolRequestUserInput(user_input(
            "call-1",
        )))
        .await;
    let (second_request_id, _second_waiter) = requester
        .send_request(ServerRequestPayload::FileChangeRequestApproval(
            file_change_approval("call-2"),
        ))
        .await;

    assert_eq!(
        [
            &dynamic_tool_request_id,
            &other_request_id,
            &first_request_id,
            &second_request_id
        ],
        [
            &RequestId::Integer(0),
            &RequestId::Integer(1),
            &RequestId::Integer(2),
            &RequestId::Integer(3)
        ]
    );
    let pending = requester.pending_requests();
    assert_eq!(
        pending.iter().map(ServerRequest::id).collect::<Vec<_>>(),
        vec![
            &dynamic_tool_request_id,
            &first_request_id,
            &second_request_id
        ]
    );
    assert_eq!(
        pending[2],
        ServerRequestPayload::FileChangeRequestApproval(file_change_approval("call-2"))
            .request_with_id(second_request_id)
    );
}

/// A request reaches the thread's connections and no other; the client's response reaches the
/// waiter, and once it has, the thread's connections are sent `serverRequest/resolved`.
#[tokio::test]
async fn a_request_goes_to_the_threads_connections_and_its_answer_resolves_it() {
    let (server, requester) = Server::start(|server| {
        ThreadRequester::new(
            server,
            &[ConnectionId::new(1), ConnectionId::new(2)],
            thread(),
        )
    });
    let mut first = server.open(1).await;
    let mut second = server.open(2).await;
    let mut outsider = server.open(3).await;
    for connection in [&mut first, &mut second, &mut outsider] {
        server.initialize(connection, false).await;
    }

    let params = file_change_approval("call-1").with_reason("needs write");
    let (request_id, answer) = requester
        .send_request(ServerRequestPayload::FileChangeRequestApproval(
            params.clone(),
        ))
        .await;
    let expected = json!({
        "id": 0,
        "method": "item/fileChange/requestApproval",
        "params": serde_json::to_value(&params).unwrap(),
    });
    assert_eq!(first.next().await, expected);
    assert_eq!(second.next().await, expected);
    server.mark("mark/afterRequest").await;
    outsider.expect_mark("mark/afterRequest").await;
    first.expect_mark("mark/afterRequest").await;
    second.expect_mark("mark/afterRequest").await;

    server
        .send(
            &second,
            JsonRpcResponse::new(0, json!({"decision": "accept"})),
        )
        .await;
    let answer = requester.wait_for_answer(request_id, answer).await;
    assert_eq!(answer, Ok(Ok(json!({"decision": "accept"}))));
    assert_eq!(
        file_change_approval_decision(answer),
        Some(ReviewDecision::Approved)
    );
    assert_eq!(first.next().await, resolved(0));
    assert_eq!(second.next().await, resolved(0));
    server.mark("mark/afterResolved").await;
    outsider.expect_mark("mark/afterResolved").await;
    assert!(requester.pending_requests().is_empty());
}

/// Codex's `notify_client_error_forwards_error_to_waiter`, through the server: the client's error
/// reaches the waiter, and the request reads as a failed approval.
#[tokio::test]
async fn notify_client_error_forwards_error_to_waiter() {
    let (server, requester) =
        Server::start(|server| ThreadRequester::new(server, &[ConnectionId::new(1)], thread()));
    let mut connection = server.open(1).await;
    server.initialize(&mut connection, false).await;

    let (request_id, answer) = requester
        .send_request(ServerRequestPayload::CommandExecutionRequestApproval(
            command_approval(),
        ))
        .await;
    connection.next().await;
    let error = JsonRpcErrorObject::internal_error("refresh failed");
    server
        .send(
            &connection,
            JsonRpcError::new(request_id.clone(), error.clone()),
        )
        .await;
    let answer = requester.wait_for_answer(request_id, answer).await;
    assert_eq!(answer, Ok(Err(error)));
    assert_eq!(
        command_execution_approval_decision(answer),
        Some(ReviewDecision::denied("approval request failed"))
    );
    assert_eq!(connection.next().await, resolved(0));
}

/// Codex's `cancel_requests_for_thread_cancels_all_thread_requests`, as a turn transition does it:
/// every request of the thread completes with the turn transition error, which needs nothing
/// passed on, and is resolved; the requests of another thread are left alone; an answer arriving
/// afterwards is ignored.
#[tokio::test]
async fn a_turn_transition_withdraws_the_threads_requests() {
    let (server, (requester, other_thread)) = Server::start(|server| {
        (
            ThreadRequester::new(server, &[ConnectionId::new(1)], thread()),
            ThreadRequester::new(server, &[ConnectionId::new(1)], SessionId::new("thread-2")),
        )
    });
    let mut connection = server.open(1).await;
    server.initialize(&mut connection, false).await;

    let (dynamic_tool_request_id, dynamic_tool_waiter) = requester
        .send_request(ServerRequestPayload::DynamicToolCall(dynamic_tool_call(
            "call-0",
        )))
        .await;
    let (user_input_request_id, user_input_waiter) = requester
        .send_request(ServerRequestPayload::ToolRequestUserInput(user_input(
            "call-1",
        )))
        .await;
    let (_, other_waiter) = other_thread
        .send_request(ServerRequestPayload::DynamicToolCall(dynamic_tool_call(
            "call-2",
        )))
        .await;
    for _ in 0..3 {
        connection.next().await;
    }

    requester.abort_pending_server_requests();

    let error = JsonRpcErrorObject::internal_error(
        "client request resolved because the turn state was changed",
    )
    .with_data(json!({"reason": "turnTransition"}));
    let dynamic_tool_answer = answer_of(dynamic_tool_waiter).await;
    assert_eq!(dynamic_tool_answer, Ok(Err(error.clone())));
    assert_eq!(dynamic_tool_call_response(dynamic_tool_answer), None);
    let user_input_answer = requester
        .wait_for_answer(user_input_request_id, user_input_waiter)
        .await;
    assert_eq!(user_input_answer, Ok(Err(error)));
    assert_eq!(request_user_input_response(user_input_answer), None);
    assert_eq!(connection.next().await, resolved(1));
    assert!(requester.pending_requests().is_empty());
    assert_eq!(other_thread.pending_requests().len(), 1);

    // The client answers a request that is no longer waiting: nothing comes back.
    server
        .send(
            &connection,
            JsonRpcResponse::new(
                dynamic_tool_request_id,
                json!({"contentItems": [], "success": true}),
            ),
        )
        .await;
    server.mark("mark/afterStrayAnswer").await;
    connection.expect_mark("mark/afterStrayAnswer").await;
    drop(other_waiter);
}

/// Codex's `command_execution_request_approval_strips_additional_permissions_without_capability`
/// and `..._keeps_additional_permissions_with_capability`, through the server: each connection is
/// sent what its own handshake allows, and `availableDecisions` is sent to both, as Codex sends it.
#[tokio::test]
async fn command_execution_request_approval_strips_additional_permissions_without_capability() {
    let (server, requester) = Server::start(|server| {
        ThreadRequester::new(
            server,
            &[ConnectionId::new(8), ConnectionId::new(9)],
            thread(),
        )
    });
    let mut stable = server.open(8).await;
    let mut experimental = server.open(9).await;
    server.initialize(&mut stable, false).await;
    server.initialize(&mut experimental, true).await;

    let params =
        command_approval()
            .with_reason("Need extra read access")
            .with_additional_permissions(AdditionalPermissionProfile::new().with_file_system(
                AdditionalFileSystemPermissions::new().with_read(["/tmp/allowed"]),
            ))
            .with_available_decisions([
                CommandExecutionApprovalDecision::Accept,
                CommandExecutionApprovalDecision::Decline,
            ]);
    requester
        .send_request(ServerRequestPayload::CommandExecutionRequestApproval(
            params,
        ))
        .await;

    let stripped = stable.next().await;
    assert_eq!(stripped["params"].get("additionalPermissions"), None);
    assert_eq!(
        stripped["params"]["availableDecisions"],
        json!(["accept", "decline"])
    );
    assert_eq!(stripped["params"]["reason"], "Need extra read access");
    let kept = experimental.next().await;
    assert_eq!(
        kept["params"]["additionalPermissions"],
        json!({"network": null, "fileSystem": {"read": ["/tmp/allowed"], "write": null}})
    );
    assert_eq!(
        kept["params"]["availableDecisions"],
        json!(["accept", "decline"])
    );
}

/// A request for a thread no connection is subscribed to waits, and a connection that attaches to
/// the thread is sent the requests still waiting, in order and as its handshake allows — Codex's
/// `replay_requests_to_connection_for_thread`. An answered request is not sent again.
#[tokio::test]
async fn requests_still_waiting_are_replayed_to_a_connection_that_attaches() {
    let (server, unsubscribed) =
        Server::start(|server| ThreadRequester::new(server, &[], thread()));
    let mut connection = server.open(1).await;
    server.initialize(&mut connection, false).await;

    let (answered_id, answered) = unsubscribed
        .send_request(ServerRequestPayload::DynamicToolCall(dynamic_tool_call(
            "call-0",
        )))
        .await;
    let (_, _approval) = unsubscribed
        .send_request(ServerRequestPayload::CommandExecutionRequestApproval(
            command_approval().with_additional_permissions(AdditionalPermissionProfile::new()),
        ))
        .await;
    let (_, _question) = unsubscribed
        .send_request(ServerRequestPayload::ToolRequestUserInput(user_input(
            "call-2",
        )))
        .await;
    server.mark("mark/nothingSent").await;
    connection.expect_mark("mark/nothingSent").await;

    // The connection answers the first request before it has seen it, as a client that resumed
    // the thread elsewhere might. The request is resolved for the thread's connections, of which
    // there are none, so nothing is sent.
    server
        .send(
            &connection,
            JsonRpcResponse::new(
                answered_id.clone(),
                json!({"contentItems": [{"type": "inputText", "text": "ok"}], "success": true}),
            ),
        )
        .await;
    let answer = unsubscribed.wait_for_answer(answered_id, answered).await;
    assert_eq!(
        dynamic_tool_call_response(answer),
        Some(DynamicToolCallResponse::new(
            [DynamicToolCallOutputContentItem::input_text("ok")],
            true
        ))
    );
    server.mark("mark/notResolvedForNobody").await;
    connection.expect_mark("mark/notResolvedForNobody").await;

    unsubscribed.replay_to_connection(connection.id).await;
    let approval = connection.next().await;
    assert_eq!(approval["id"], 1);
    assert_eq!(approval["method"], "item/commandExecution/requestApproval");
    assert_eq!(approval["params"].get("additionalPermissions"), None);
    let question = connection.next().await;
    assert_eq!(question["id"], 2);
    assert_eq!(question["method"], "item/tool/requestUserInput");
    server.mark("mark/afterReplay").await;
    connection.expect_mark("mark/afterReplay").await;
}

/// Like Codex's resume approval tests and turn interruption test, a replayed approval stays part
/// of the thread. Its answer, error or turn transition resolves it for the subscribers that
/// attached later, even if the request was originally raised with no subscribers.
#[tokio::test]
async fn replayed_approvals_resolve_for_current_subscribers() {
    for initially_subscribed in [false, true] {
        for completion in ["answer", "error", "turn_transition"] {
            let initial = if initially_subscribed {
                vec![ConnectionId::new(1)]
            } else {
                Vec::new()
            };
            let (server, (requester, _other_thread)) = Server::start(|server| {
                (
                    ThreadRequester::new(server, &initial, thread()),
                    ThreadRequester::new(
                        server,
                        &[ConnectionId::new(3)],
                        SessionId::new("thread-2"),
                    ),
                )
            });
            let mut original = server.open(1).await;
            let mut resumed = server.open(2).await;
            let mut outsider = server.open(3).await;
            server.initialize(&mut original, false).await;
            server.initialize(&mut resumed, false).await;
            server.initialize(&mut outsider, false).await;
            let (id, answer) = requester
                .send_request(ServerRequestPayload::CommandExecutionRequestApproval(
                    command_approval(),
                ))
                .await;
            if initially_subscribed {
                assert_eq!(original.next().await["id"], 0);
            }
            server.mark("mark/beforeReplay").await;
            original.expect_mark("mark/beforeReplay").await;
            resumed.expect_mark("mark/beforeReplay").await;
            outsider.expect_mark("mark/beforeReplay").await;
            requester.replay_to_connection(resumed.id).await;
            assert_eq!(resumed.next().await["id"], 0);

            let expected = match completion {
                "answer" => {
                    server
                        .send(
                            &resumed,
                            JsonRpcResponse::new(
                                id.clone(),
                                json!({"decision": {
                                    "acceptWithExecpolicyAmendment": {
                                        "execpolicy_amendment": ["git", "status"],
                                    },
                                }}),
                            ),
                        )
                        .await;
                    Some(ReviewDecision::ApprovedExecpolicyAmendment {
                        proposed_execpolicy_amendment: ExecPolicyAmendment::new(["git", "status"]),
                    })
                }
                "error" => {
                    server
                        .send(
                            &resumed,
                            JsonRpcError::new(
                                id.clone(),
                                JsonRpcErrorObject::internal_error("boom"),
                            ),
                        )
                        .await;
                    Some(ReviewDecision::denied("approval request failed"))
                }
                "turn_transition" => {
                    requester.abort_pending_server_requests();
                    None
                }
                _ => unreachable!(),
            };
            let result = requester.wait_for_answer(id, answer).await;
            assert_eq!(command_execution_approval_decision(result), expected);
            assert_eq!(resumed.next().await, resolved(0));
            if initially_subscribed {
                assert_eq!(original.next().await, resolved(0));
            }
            assert!(requester.pending_requests().is_empty());
            server.mark("mark/afterResolve").await;
            original.expect_mark("mark/afterResolve").await;
            resumed.expect_mark("mark/afterResolve").await;
            outsider.expect_mark("mark/afterResolve").await;
        }
    }
}

/// Resolution consults the thread's current subscriptions, not a request's original snapshot.
/// Removing one subscriber does not remove the other or subscriptions belonging to another thread.
#[tokio::test]
async fn unsubscribed_connections_do_not_receive_request_resolution() {
    let (server, (requester, other_thread)) = Server::start(|server| {
        (
            ThreadRequester::new(
                server,
                &[ConnectionId::new(1), ConnectionId::new(2)],
                thread(),
            ),
            ThreadRequester::new(server, &[ConnectionId::new(1)], SessionId::new("thread-2")),
        )
    });
    let mut first = server.open(1).await;
    let mut second = server.open(2).await;
    server.initialize(&mut first, false).await;
    server.initialize(&mut second, false).await;
    let (id, answer) = requester
        .send_request(ServerRequestPayload::FileChangeRequestApproval(
            file_change_approval("call-1"),
        ))
        .await;
    first.next().await;
    second.next().await;
    assert!(requester.unsubscribe_connection(first.id));
    assert!(!requester.unsubscribe_connection(first.id));
    server
        .send(
            &second,
            JsonRpcResponse::new(id.clone(), json!({"decision": "acceptForSession"})),
        )
        .await;
    let result = requester.wait_for_answer(id, answer).await;
    assert_eq!(
        file_change_approval_decision(result),
        Some(ReviewDecision::ApprovedForSession)
    );
    assert_eq!(second.next().await, resolved(0));
    server.mark("mark/unsubscribed").await;
    first.expect_mark("mark/unsubscribed").await;
    second.expect_mark("mark/unsubscribed").await;

    let (id, answer) = other_thread
        .send_request(ServerRequestPayload::FileChangeRequestApproval(
            FileChangeRequestApprovalParams::new("thread-2", "turn-1", "call-2", 0),
        ))
        .await;
    assert_eq!(first.next().await["id"], 1);
    server
        .send(
            &first,
            JsonRpcResponse::new(id.clone(), json!({"decision": "accept"})),
        )
        .await;
    other_thread
        .wait_for_answer(id, answer)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        first.next().await,
        json!({
            "method": SERVER_REQUEST_RESOLVED_METHOD,
            "params": {"threadId": "thread-2", "requestId": 1},
        })
    );
    server.mark("mark/otherThreadResolved").await;
    second.expect_mark("mark/otherThreadResolved").await;
}

/// Closing a connection removes all its subscriptions. Reusing its transport id does not attach
/// the replacement connection to the old threads.
#[tokio::test]
async fn closed_connections_are_removed_from_every_threads_subscriptions() {
    let (server, (requester, other_thread)) = Server::start(|server| {
        (
            ThreadRequester::new(server, &[ConnectionId::new(1)], thread()),
            ThreadRequester::new(server, &[ConnectionId::new(1)], SessionId::new("thread-2")),
        )
    });
    let mut original = server.open(1).await;
    let mut resumed = server.open(2).await;
    server.initialize(&mut original, false).await;
    server.initialize(&mut resumed, false).await;
    let (first_id, first_answer) = requester
        .send_request(ServerRequestPayload::FileChangeRequestApproval(
            file_change_approval("call-1"),
        ))
        .await;
    let (other_id, other_answer) = other_thread
        .send_request(ServerRequestPayload::FileChangeRequestApproval(
            FileChangeRequestApprovalParams::new("thread-2", "turn-1", "call-2", 0),
        ))
        .await;
    original.next().await;
    original.next().await;
    requester.replay_to_connection(resumed.id).await;
    other_thread.replay_to_connection(resumed.id).await;
    resumed.next().await;
    resumed.next().await;
    server
        .events
        .send(TransportEvent::ConnectionClosed {
            connection_id: original.id,
        })
        .await
        .unwrap();
    let mut replacement = server.open(1).await;
    server.initialize(&mut replacement, false).await;
    for id in [&first_id, &other_id] {
        server
            .send(
                &resumed,
                JsonRpcResponse::new(id.clone(), json!({"decision": "accept"})),
            )
            .await;
    }
    requester
        .wait_for_answer(first_id, first_answer)
        .await
        .unwrap()
        .unwrap();
    other_thread
        .wait_for_answer(other_id, other_answer)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resumed.next().await, resolved(0));
    assert_eq!(
        resumed.next().await,
        json!({
            "method": SERVER_REQUEST_RESOLVED_METHOD,
            "params": {"threadId": "thread-2", "requestId": 1},
        })
    );
    server.mark("mark/afterClose").await;
    replacement.expect_mark("mark/afterClose").await;
    resumed.expect_mark("mark/afterClose").await;
}

/// A request that cannot be written as JSON — here a path that is not UTF-8 — is not sent, and
/// completes at once with an internal error, which its waiter reads as a failed approval.
#[cfg(unix)]
#[tokio::test]
async fn a_request_that_cannot_be_written_fails_at_once() {
    use std::{ffi::OsStr, os::unix::ffi::OsStrExt, path::PathBuf};

    let (server, requester) =
        Server::start(|server| ThreadRequester::new(server, &[ConnectionId::new(1)], thread()));
    let mut connection = server.open(1).await;
    server.initialize(&mut connection, false).await;

    let params = file_change_approval("call-1")
        .with_grant_root(PathBuf::from(OsStr::from_bytes(b"/tmp/\xff")));
    let (request_id, answer) = requester
        .send_request(ServerRequestPayload::FileChangeRequestApproval(params))
        .await;
    assert_eq!(request_id, RequestId::Integer(0));
    let answer = answer_of(answer).await;
    let Ok(Err(error)) = &answer else {
        panic!("an internal error: {answer:?}");
    };
    assert_eq!(error.code(), -32603);
    assert!(
        error.message().starts_with("failed to serialize request: "),
        "{error}"
    );
    assert_eq!(
        file_change_approval_decision(answer),
        Some(ReviewDecision::denied("approval request failed"))
    );
    assert!(requester.pending_requests().is_empty());
    server.mark("mark/nothingSent").await;
    connection.expect_mark("mark/nothingSent").await;
}

// ---------------------------------------------------------------------------------------------
// What the server makes of the answers
// ---------------------------------------------------------------------------------------------

async fn closed_without_answer() -> ClientAnswer {
    let (sender, receiver) = oneshot::channel::<RequestResult>();
    drop(sender);
    receiver.await
}

fn turn_transition() -> ClientAnswer {
    Ok(Err(JsonRpcErrorObject::internal_error(
        "client request resolved because the turn state was changed",
    )
    .with_data(json!({"reason": "turnTransition"}))))
}

fn client_error() -> ClientAnswer {
    Ok(Err(
        JsonRpcErrorObject::internal_error("boom").with_data(json!({"reason": "other"}))
    ))
}

fn answered(value: Value) -> ClientAnswer {
    Ok(Ok(value))
}

/// Codex's `on_command_execution_request_approval_response`: a refusal is passed on as one, a
/// cancellation stops the turn, and anything unusable is refused as a failed request. Session
/// approvals and both kinds of amendment retain their scope and payload for the coding runtime.
#[tokio::test]
async fn command_approval_answers_preserve_scope_and_policy_amendments() {
    let allow = Some(ReviewDecision::Approved);
    let rejected = Some(ReviewDecision::denied("rejected by user"));
    let failed = Some(ReviewDecision::denied("approval request failed"));
    let cases = [
        (json!({"decision": "accept"}), allow.clone()),
        (
            json!({"decision": "acceptForSession"}),
            Some(ReviewDecision::ApprovedForSession),
        ),
        (
            json!({"decision": {"acceptWithExecpolicyAmendment": {"execpolicy_amendment": ["git", "status"]}}}),
            Some(ReviewDecision::ApprovedExecpolicyAmendment {
                proposed_execpolicy_amendment: ExecPolicyAmendment::new(["git", "status"]),
            }),
        ),
        (
            json!({"decision": {"applyNetworkPolicyAmendment": {
                "network_policy_amendment": {"host": "example.com", "action": "allow"},
            }}}),
            Some(ReviewDecision::NetworkPolicyAmendment {
                network_policy_amendment: NetworkPolicyAmendment::new(
                    "example.com",
                    NetworkPolicyRuleAction::Allow,
                ),
            }),
        ),
        (
            json!({"decision": {"applyNetworkPolicyAmendment": {
                "network_policy_amendment": {"host": "example.com", "action": "deny"},
            }}}),
            Some(ReviewDecision::NetworkPolicyAmendment {
                network_policy_amendment: NetworkPolicyAmendment::new(
                    "example.com",
                    NetworkPolicyRuleAction::Deny,
                ),
            }),
        ),
        (json!({"decision": "decline"}), rejected),
        (json!({"decision": "cancel"}), Some(ReviewDecision::Abort)),
        (json!({"decision": "approve"}), failed.clone()),
        (json!({}), failed.clone()),
    ];
    for (value, expected) in cases {
        assert_eq!(
            command_execution_approval_decision(answered(value.clone())),
            expected,
            "{value}"
        );
    }
    assert_eq!(
        command_execution_approval_decision(client_error()),
        failed.clone()
    );
    assert_eq!(
        command_execution_approval_decision(closed_without_answer().await),
        failed
    );
    assert_eq!(command_execution_approval_decision(turn_transition()), None);
}

/// Codex's `on_file_change_request_approval_response` and `map_file_change_approval_decision`.
#[tokio::test]
async fn file_change_approval_answers_preserve_session_scope() {
    let failed = Some(ReviewDecision::denied("approval request failed"));
    let cases = [
        ("accept", Some(ReviewDecision::Approved)),
        ("acceptForSession", Some(ReviewDecision::ApprovedForSession)),
        ("decline", Some(ReviewDecision::denied("rejected by user"))),
        ("cancel", Some(ReviewDecision::Abort)),
        ("acceptWithExecpolicyAmendment", failed.clone()),
    ];
    for (decision, expected) in cases {
        assert_eq!(
            file_change_approval_decision(answered(json!({"decision": decision}))),
            expected,
            "{decision}"
        );
    }
    assert_eq!(
        file_change_approval_decision(client_error()),
        failed.clone()
    );
    assert_eq!(
        file_change_approval_decision(closed_without_answer().await),
        failed
    );
    assert_eq!(file_change_approval_decision(turn_transition()), None);
}

/// Codex's `on_request_user_input_response`: without a usable answer the questions are answered
/// with nothing.
#[tokio::test]
async fn user_input_answers_fall_back_to_no_answers() {
    let answers = ToolRequestUserInputResponse::new()
        .with_answer("q1", ToolRequestUserInputAnswer::new(["yes"]));
    assert_eq!(
        request_user_input_response(answered(serde_json::to_value(&answers).unwrap())),
        Some(answers)
    );
    let nothing = Some(ToolRequestUserInputResponse::new());
    assert_eq!(
        request_user_input_response(answered(json!({"answers": [1]}))),
        nothing
    );
    assert_eq!(request_user_input_response(client_error()), nothing);
    assert_eq!(
        request_user_input_response(closed_without_answer().await),
        nothing
    );
    assert_eq!(request_user_input_response(turn_transition()), None);
}

/// Codex's `request_permissions_response_from_client_result`, with its
/// `request_permissions_response_preserves_session_scope`,
/// `..._rejects_session_scoped_strict_auto_review` and
/// `..._preserves_turn_scoped_strict_auto_review`: without a usable answer nothing is granted.
#[tokio::test]
async fn permission_answers_fall_back_to_granting_nothing() {
    let session = PermissionsRequestApprovalResponse::new(
        GrantedPermissionProfile::new()
            .with_network(AdditionalNetworkPermissions::new().with_enabled(true)),
    )
    .with_scope(PermissionGrantScope::Session);
    assert_eq!(
        request_permissions_response(answered(serde_json::to_value(&session).unwrap())),
        Some(session.clone())
    );
    let strict_turn = PermissionsRequestApprovalResponse::new(GrantedPermissionProfile::new())
        .with_strict_auto_review(true);
    assert_eq!(
        request_permissions_response(answered(serde_json::to_value(&strict_turn).unwrap())),
        Some(strict_turn)
    );

    let nothing = Some(PermissionsRequestApprovalResponse::new(
        GrantedPermissionProfile::new(),
    ));
    let strict_session = session.with_strict_auto_review(true);
    assert_eq!(
        request_permissions_response(answered(serde_json::to_value(&strict_session).unwrap())),
        nothing
    );
    assert_eq!(
        request_permissions_response(answered(json!({"permissions": {"macos": {}}}))),
        nothing
    );
    assert_eq!(request_permissions_response(client_error()), nothing);
    assert_eq!(
        request_permissions_response(closed_without_answer().await),
        nothing
    );
    assert_eq!(request_permissions_response(turn_transition()), None);
}

/// Codex's `dynamic_tools::on_call_response` and `decode_response`: a remote image or audio that is
/// not inline fails the call with the reason, as does an answer that does not fit or a failed
/// request.
#[tokio::test]
async fn dynamic_tool_answers_refuse_remote_media_and_fall_back_to_failure() {
    let failure = |text: &str| {
        Some(DynamicToolCallResponse::new(
            [DynamicToolCallOutputContentItem::input_text(text)],
            false,
        ))
    };
    let inline = DynamicToolCallResponse::new(
        [
            DynamicToolCallOutputContentItem::input_image("data:image/png;base64,AAA"),
            DynamicToolCallOutputContentItem::input_audio("DATA:audio/wav;base64,AAA"),
        ],
        false,
    );
    assert_eq!(
        dynamic_tool_call_response(answered(serde_json::to_value(&inline).unwrap())),
        Some(inline)
    );
    let remote_image = json!({
        "contentItems": [{"type": "inputImage", "imageUrl": "HTTPS://example.com/a.png"}],
        "success": true,
    });
    assert_eq!(
        dynamic_tool_call_response(answered(remote_image)),
        failure("remote image URLs are not supported; use an inline data URL instead")
    );
    let remote_audio = json!({
        "contentItems": [{"type": "inputAudio", "audioUrl": "file:///tmp/a.wav"}],
        "success": true,
    });
    assert_eq!(
        dynamic_tool_call_response(answered(remote_audio)),
        failure("audio URLs must use an inline data URL")
    );
    assert_eq!(
        dynamic_tool_call_response(answered(json!({"success": true}))),
        failure("dynamic tool response was invalid")
    );
    assert_eq!(
        dynamic_tool_call_response(client_error()),
        failure("dynamic tool request failed")
    );
    assert_eq!(
        dynamic_tool_call_response(closed_without_answer().await),
        failure("dynamic tool request failed")
    );
    assert_eq!(dynamic_tool_call_response(turn_transition()), None);
}

/// Codex's `ExecApprovalRequestEvent::default_available_decisions`: a network approval offers
/// approving once or for the session, the first proposed rule that allows the host, and
/// cancelling; one asking for more permissions, approving once and cancelling; any other,
/// approving once, its proposed command-prefix rule, and cancelling.
#[test]
fn default_available_decisions_follow_codex() {
    let network = NetworkApprovalContext::new("example.com", NetworkApprovalProtocol::Https);
    let rules = [
        NetworkPolicyAmendment::new("example.com", NetworkPolicyRuleAction::Deny),
        NetworkPolicyAmendment::new("example.com", NetworkPolicyRuleAction::Allow),
    ];
    let amendment = ExecPolicyAmendment::new(["git", "pull"]);
    assert_eq!(
        CommandExecutionApprovalDecision::default_available(
            Some(&network),
            Some(&amendment),
            Some(&rules),
            None,
        ),
        [
            CommandExecutionApprovalDecision::Accept,
            CommandExecutionApprovalDecision::AcceptForSession,
            CommandExecutionApprovalDecision::apply_network_policy_amendment(rules[1].clone()),
            CommandExecutionApprovalDecision::Cancel,
        ]
    );
    assert_eq!(
        CommandExecutionApprovalDecision::default_available(
            None,
            Some(&amendment),
            None,
            Some(&AdditionalPermissionProfile::new()),
        ),
        [
            CommandExecutionApprovalDecision::Accept,
            CommandExecutionApprovalDecision::Cancel,
        ]
    );
    assert_eq!(
        CommandExecutionApprovalDecision::default_available(None, Some(&amendment), None, None),
        [
            CommandExecutionApprovalDecision::Accept,
            CommandExecutionApprovalDecision::accept_with_execpolicy_amendment(amendment.clone()),
            CommandExecutionApprovalDecision::Cancel,
        ]
    );
    assert_eq!(
        CommandExecutionApprovalDecision::default_available(None, None, None, None),
        [
            CommandExecutionApprovalDecision::Accept,
            CommandExecutionApprovalDecision::Cancel,
        ]
    );
}
