//! The questions a thread's tools ask the user: the `RequestUserInput` arm of Codex's
//! `apply_bespoke_event_handling` and its `on_request_user_input_response`.
//!
//! Every loaded thread gives its turns a [`UserInputPort`] (see [`install_user_input`]). A call of
//! `request_user_input` hands the port its questions; the port queues them for the thread listener,
//! which keeps the thread waiting on the user's input, sends `item/tool/requestUserInput` to the
//! thread's connections, and, in a task of its own, waits for the answer, reports the request
//! resolved, stops waiting, and hands the answer read as Codex reads it (see [`super::answers`])
//! back to the call. A request the client answers with an error, or with something that is not an
//! answer, is answered with nothing; one withdrawn because its turn moved on passes nothing on, and
//! the call ends cancelled.
//!
//! # Differences from Codex
//!
//! Codex's session raises the request as an event, which its listener turns into the server
//! request, and the answer travels back as an operation keyed by the turn. Codex keeps that event
//! out of its rollout; host events here are always recorded, so the port uses an ephemeral listener
//! command. The listener drains preceding turn events before commands, preserving the request's
//! order after its turn starts and its call is reported. The answer goes straight back to the call
//! through a Rust oneshot rather than a session operation keyed by the turn.

use std::sync::Arc;

use async_trait::async_trait;
use ra_core::{
    context::RunContext,
    item::CallId,
    session::SessionId,
    user_input::{
        RequestUserInputAnswer, RequestUserInputArgs, RequestUserInputQuestion,
        RequestUserInputResponse, UserInputPort,
    },
};
use ra_runtime::thread::AgentThreadBuilder;
use tokio::sync::oneshot;

use super::{
    answers::request_user_input_response,
    outgoing::{OutgoingMessageSender, ThreadScopedOutgoingMessageSender},
    protocol::{
        ServerRequestPayload, ToolRequestUserInputOption, ToolRequestUserInputParams,
        ToolRequestUserInputQuestion, ToolRequestUserInputResponse,
    },
    thread_state::ThreadListenerCommand,
    thread_status::ThreadWatchManager,
};

/// Where one loaded thread's questions for the user go.
struct ThreadUserInput {
    outgoing: Arc<OutgoingMessageSender>,
    thread_id: SessionId,
}

#[async_trait]
impl UserInputPort for ThreadUserInput {
    async fn request_user_input(
        &self,
        run: &RunContext,
        call_id: &CallId,
        args: RequestUserInputArgs,
    ) -> Option<RequestUserInputResponse> {
        let mut params = ToolRequestUserInputParams::new(
            self.thread_id.as_str(),
            run.run_id().as_str(),
            call_id.as_str(),
            args.questions().iter().map(wire_question),
        )
        .with_is_blocking(args.is_blocking());
        if let Some(auto_resolution_ms) = args.auto_resolution_ms() {
            params = params.with_auto_resolution_ms(auto_resolution_ms);
        }
        let state = self.outgoing.thread_state().thread_state(&self.thread_id);
        let commands = state.lock().await.listener_command_tx()?;
        let (sender, receiver) = oneshot::channel();
        commands
            .send(ThreadListenerCommand::RequestUserInput {
                params,
                response_tx: sender,
            })
            .ok()?;
        receiver.await.ok()
    }
}

/// Sends a question in the listener's turn order, then waits independently so the listener can
/// still process interruption and connection changes, as Codex's bespoke event handler does.
pub(crate) async fn request_user_input(
    params: ToolRequestUserInputParams,
    response_tx: oneshot::Sender<RequestUserInputResponse>,
    outgoing: ThreadScopedOutgoingMessageSender,
    thread_watch: &Arc<ThreadWatchManager>,
    thread_id: &SessionId,
) {
    let user_input_guard = thread_watch.note_user_input_requested(thread_id).await;
    let (request_id, answer) = outgoing
        .send_request(ServerRequestPayload::ToolRequestUserInput(params))
        .await;
    tokio::spawn(async move {
        let answer = outgoing.wait_for_answer(request_id, answer).await;
        drop(user_input_guard);
        if let Some(response) = request_user_input_response(answer) {
            let _ = response_tx.send(core_response(&response));
        }
    });
}

/// A question as the request carries it: the question conversion in Codex's handler.
fn wire_question(question: &RequestUserInputQuestion) -> ToolRequestUserInputQuestion {
    let wire =
        ToolRequestUserInputQuestion::new(question.id(), question.header(), question.question())
            .with_is_other(question.is_other())
            .with_is_secret(question.is_secret());
    match question.options() {
        Some(options) => {
            wire.with_options(options.iter().map(|option| {
                ToolRequestUserInputOption::new(option.label(), option.description())
            }))
        }
        None => wire,
    }
}

/// The answers as the call is given them: the response conversion in Codex's
/// `on_request_user_input_response`.
fn core_response(response: &ToolRequestUserInputResponse) -> RequestUserInputResponse {
    response
        .answers()
        .iter()
        .fold(RequestUserInputResponse::new(), |answers, (id, answer)| {
            answers.with_answer(
                id.clone(),
                RequestUserInputAnswer::new(answer.answers().iter().cloned()),
            )
        })
}

/// Gives every turn of the thread `builder` prepares a port that asks its connections' user.
pub(crate) fn install_user_input(
    builder: AgentThreadBuilder,
    outgoing: &Arc<OutgoingMessageSender>,
) -> AgentThreadBuilder {
    let port = ThreadUserInput {
        outgoing: Arc::clone(outgoing),
        thread_id: builder.thread_id().clone(),
    };
    let services = builder.services().clone().with_user_input(Arc::new(port));
    builder.with_services(services)
}
