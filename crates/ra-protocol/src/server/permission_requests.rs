//! The permission requests a thread's product makes of its client: the `RequestPermissions` arm of
//! Codex's `apply_bespoke_event_handling` and its `on_request_permissions_response`.
//!
//! Codex's permission profile belongs to its coding product, so the request is made by the product
//! rather than by a tool of the framework's. Every loaded thread hands its product a
//! [`ThreadServerRequests`] (see [`ThreadSettingsAdapter::attach_server_requests`]); a request made
//! through it is queued for the thread listener, which drops it if the call or its turn is gone,
//! keeps the thread waiting on approval, and sends `item/permissions/requestApproval`. A task of
//! its own then waits for the answer, reports the request resolved, stops waiting, and reads the
//! answer as Codex reads it (see [`super::answers`]). A grant naming a path that is not an absolute
//! local path fails the turn: the thread's connections are sent Codex's error and the turn is
//! interrupted, as Codex's handler does when it cannot localize a granted path.
//!
//! # Differences from Codex
//!
//! - Codex's session raises the request as an event that is kept out of its rollout; host events
//!   here are always recorded, so the request travels as an ephemeral listener command, as the
//!   questions of `request_user_input` do.
//! - A granted path is localized as an absolute native path, `~` naming the home directory;
//!   Codex's fallback to `file:` URIs is not ported.
//!
//! [`ThreadSettingsAdapter::attach_server_requests`]: super::ThreadSettingsAdapter::attach_server_requests

use std::{fmt, sync::Arc};

use ra_core::session::SessionId;
use ra_runtime::thread::AgentThreadBuilder;
use tokio::sync::{Mutex, oneshot};

use super::{
    answers::request_permissions_response,
    outgoing::{OutgoingMessageSender, ServerNotification, ThreadScopedOutgoingMessageSender},
    protocol::{
        AdditionalFileSystemPermissions, ERROR_METHOD, ErrorNotification, FileSystemPath,
        PermissionsRequestApprovalParams, PermissionsRequestApprovalResponse, ServerRequestPayload,
        TurnError,
    },
    settings_adapter::ThreadSettingsAdapter,
    thread_manager::LoadedThread,
    thread_state::{ThreadListenerCommand, ThreadState},
    thread_status::ThreadWatchManager,
};

/// A loaded thread's way of asking its client: what Codex's session reaches its app server through.
#[derive(Clone)]
pub struct ThreadServerRequests {
    outgoing: Arc<OutgoingMessageSender>,
    thread_id: SessionId,
}

impl fmt::Debug for ThreadServerRequests {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ThreadServerRequests")
            .field("thread_id", &self.thread_id)
            .finish_non_exhaustive()
    }
}

impl ThreadServerRequests {
    pub(crate) fn new(outgoing: &Arc<OutgoingMessageSender>, thread_id: &SessionId) -> Self {
        Self {
            outgoing: Arc::clone(outgoing),
            thread_id: thread_id.clone(),
        }
    }

    /// The thread the requests are made for.
    #[must_use]
    pub fn thread_id(&self) -> &str {
        self.thread_id.as_str()
    }

    /// Asks the client for the permissions `params` request and waits for the grant: Codex's
    /// `RequestPermissions` event and its response handler.
    ///
    /// `None` means no grant will come: the call or its turn was gone before the request was sent,
    /// the request was withdrawn because its turn moved on, or the grant could not be localized and
    /// the turn was failed. A request the client could not answer is answered with nothing granted,
    /// for the turn.
    pub async fn request_permissions(
        &self,
        params: PermissionsRequestApprovalParams,
    ) -> Option<PermissionsRequestApprovalResponse> {
        let state = self.outgoing.thread_state().thread_state(&self.thread_id);
        let commands = state.lock().await.listener_command_tx()?;
        let (sender, receiver) = oneshot::channel();
        commands
            .send(ThreadListenerCommand::RequestPermissions {
                params,
                response_tx: sender,
            })
            .ok()?;
        receiver.await.ok()
    }
}

/// Sends a permission request in the listener's turn order, then waits for the answer in a task of
/// its own, so the listener goes on handling interruption and connection changes, as Codex's
/// handler does.
pub(crate) async fn request_permissions(
    params: PermissionsRequestApprovalParams,
    response_tx: oneshot::Sender<PermissionsRequestApprovalResponse>,
    outgoing: ThreadScopedOutgoingMessageSender,
    thread_watch: &Arc<ThreadWatchManager>,
    loaded: LoadedThread,
    thread_state: Arc<Mutex<ThreadState>>,
) {
    let permission_guard = thread_watch
        .note_permission_requested(loaded.thread_id())
        .await;
    let turn_id = params.turn_id().to_owned();
    let (request_id, answer) = outgoing
        .send_request(ServerRequestPayload::PermissionsRequestApproval(params))
        .await;
    tokio::spawn(async move {
        let answer = outgoing.wait_for_answer(request_id, answer).await;
        drop(permission_guard);
        let Some(response) = request_permissions_response(answer) else {
            return;
        };
        if let Err(error) = localize_granted_paths(&response) {
            let error = TurnError::new(format!(
                "failed to localize granted filesystem paths: {error}"
            ));
            thread_state.lock().await.turn_summary.last_error = Some(error.clone());
            match serde_json::to_value(ErrorNotification::new(
                error,
                false,
                loaded.thread_id().as_str(),
                turn_id,
            )) {
                Ok(params) => {
                    outgoing
                        .send_server_notification(ServerNotification::new(ERROR_METHOD, params))
                        .await;
                }
                Err(error) => tracing::error!("failed to serialize {ERROR_METHOD}: {error}"),
            }
            if !loaded.thread.interrupt() {
                tracing::error!("failed to interrupt turn after invalid permission paths");
            }
            return;
        }
        let _ = response_tx.send(response);
    });
}

/// Checks that every path `response` grants is an absolute local path: Codex's conversion of a
/// granted profile into its core paths.
fn localize_granted_paths(response: &PermissionsRequestApprovalResponse) -> Result<(), String> {
    let Some(file_system) = response.permissions().file_system() else {
        return Ok(());
    };
    granted_path_strings(file_system).try_for_each(|path| {
        crate::absolute_path::from_absolute_path_checked(std::path::Path::new(path))
            .map(|_| ())
            .map_err(|error| format!("invalid native path `{path}`: {error}"))
    })
}

/// The path strings a granted file system profile names: its entries when it has them, its read
/// and write lists otherwise, as Codex reads it.
fn granted_path_strings(
    file_system: &AdditionalFileSystemPermissions,
) -> Box<dyn Iterator<Item = &str> + '_> {
    match file_system.entries() {
        Some(entries) => Box::new(entries.iter().filter_map(|entry| match entry.path() {
            FileSystemPath::Path { path, .. } => Some(path.as_str()),
            _ => None,
        })),
        None => Box::new(
            file_system
                .read()
                .into_iter()
                .flatten()
                .chain(file_system.write().into_iter().flatten())
                .map(String::as_str),
        ),
    }
}

/// Hands the product of the thread `builder` prepares, if it has one, the way to ask its client.
pub(crate) fn attach_server_requests(
    builder: &AgentThreadBuilder,
    adapter: Option<&dyn ThreadSettingsAdapter>,
    outgoing: &Arc<OutgoingMessageSender>,
) {
    if let Some(adapter) = adapter {
        adapter.attach_server_requests(
            builder.config(),
            ThreadServerRequests::new(outgoing, builder.thread_id()),
        );
    }
}
