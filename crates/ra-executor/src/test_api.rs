//! Hidden bridges to the real production algorithms used by source tests.
#![allow(missing_docs, reason = "Hidden source-test facade access.")]
pub mod connection {
    pub use crate::client_inbound_request_limit::{
        MAX_CLIENT_INBOUND_NOTIFICATION_LEN, MAX_CLIENT_INBOUND_REQUEST_LEN,
        client_inbound_message_exceeded_limit,
    };
    pub use crate::connection::{
        JsonRpcConnection, JsonRpcConnectionEvent, JsonRpcTransport, JsonRpcWebSocketFrame,
        JsonRpcWebSocketMessage,
    };
}
pub mod rpc {
    pub use crate::rpc::{
        MAX_IN_FLIGHT_REGULAR_CALLS, RESERVED_OUTBOUND_CONTROL_MESSAGES, RpcCallError, RpcClient,
        RpcClientEvent, RpcNotificationSender, RpcServerOutboundMessage, invalid_request,
    };
}
pub mod rpc_server_requests {
    pub use crate::rpc::RpcServerOutboundMessage;
    pub use crate::rpc_server_requests::*;
}
pub mod client_inbound_request_limit {
    pub use crate::client_inbound_request_limit::*;
}
pub mod environment_toml {
    pub use crate::client_api::*;
    pub use crate::environment_provider::{
        EnvironmentDefault, EnvironmentProviderEntry, EnvironmentProviderSnapshot,
    };
    pub use crate::environment_toml::{
        ENVIRONMENTS_TOML_FILE, EnvironmentToml, EnvironmentsToml, MAX_ENVIRONMENT_ID_LEN,
        TomlEnvironmentProvider, environment_provider_from_codex_home, load_environments_toml,
    };
    pub use crate::{EnvironmentManager, LOCAL_ENVIRONMENT_ID, REMOTE_ENVIRONMENT_ID};
}
pub mod environment_provider {
    pub use crate::environment_provider::*;
}
pub mod client_api {
    pub use crate::client_api::*;
}
pub mod environment {
    pub use crate::ExecServerRuntimeOptions;
    pub use crate::client_api::*;
    pub use crate::environment::from_snapshot_for_test as from_snapshot;
    pub use crate::environment::*;
    pub use crate::environment_provider::*;
    pub use crate::local_environment_info::local_environment_info;
}
pub mod remote_file_system {
    pub use crate::client::LazyRemoteExecServerClient;
    pub use crate::remote_file_system::*;
    pub use crate::{
        CopyOptions, CreateDirectoryOptions, Environment, ExecutorFileSystem, FileMetadata,
        FileSystemSandboxContext, GetMetadataOptions, ReadFileOptions, RemoveOptions,
        WriteFileOptions,
    };
}

pub mod client {
    pub use crate::ExecServerError;
    pub use crate::client::{
        ExecServerClient, LazyRemoteExecServerClient,
        connect_for_transport_for_test as connect_for_transport,
        is_disconnected_for_test as is_disconnected, is_retryable_recovery_error,
        is_retryable_registry_error,
    };
    pub use crate::client_api::*;
}
