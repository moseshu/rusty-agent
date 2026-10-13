//! Calls of the tools a client provides: Codex's dynamic tool handler
//! (`core/src/tools/handlers/dynamic.rs`), its registration (`append_dynamic_tool_runtimes` in
//! `core/src/tools/spec_plan.rs`) and the app server's side of the call (the `DynamicToolCall`
//! arm of `bespoke_event_handling.rs`, `dynamic_tools.rs`).
//!
//! # Registration
//!
//! A thread started with `dynamicTools` gives its agent one tool per function, after the tools
//! the agent already has: a function named like one of them — or like a handoff — is skipped, as
//! Codex's registry skips an external tool it already holds, and so is a top-level one named
//! `exec_command` or `shell_command`, which Codex reserves. A function in a namespace is a tool in
//! that namespace: the model sees the namespace, described as the client describes it or as Codex
//! does when it is not, and the call names both, so it is routed by namespace and name (see
//! [`ToolOrigin::namespaced`]). Each function is described to the model as Codex describes it: its
//! input schema read into the schema subset (see [`super::json_schema`]), not strict. The list is
//! recorded in the thread's session metadata, and a thread resumed or forked takes it from its
//! history, as Codex's sessions do.
//!
//! # A call
//!
//! The tool waits inside its invocation for the client to answer, as Codex's handler does —
//! agents-python has no contract for tools a client runs, so there is none to keep. It records a
//! request event, which the thread's listener turns into the call's `item/started` and then the
//! `item/tool/call` request, in that order, as Codex's event handler does. The answer, read as
//! Codex reads it (see [`super::answers`]), is handed back to the waiting call; the call records a
//! response event, which completes the item, and the model is given what the client returned.
//! Both events go into the rollout, so the item reads back as it was sent. As in Codex, the request
//! is not reported resolved once answered. A call whose turn is interrupted before the answer
//! comes ends failed with nothing returned, its end recording Codex's text as the error, and a
//! request withdrawn on a turn transition passes nothing on.
//!
//! # Differences from Codex
//!
//! - Codex keeps its live items in the rollout only in its paginated history mode; its legacy
//!   rollouts drop dynamic tool calls. The two events here are this port's counterpart of the
//!   former, as the command boundaries of [`super::thread_history`] are.
//! - Codex sends a lone text item to the model as a plain string; the model here is given what
//!   every tool result is lowered to by its provider adapter.
//! - A result with no content items is given to the model as empty text, since a tool result here
//!   cannot be empty.
//! - Audio in a result is refused with a text of Rusty's own, until tool results can carry it.
//! - Deferred functions are not registered: `thread/start` refuses them, and those a history
//!   records are skipped.
//! - A namespace holding a function of its own name is refused before thread creation, as the
//!   framework reserves that shape for deferred top-level tools (see
//!   [`ra_core::tool::ToolLookupKey::namespaced`]); unsupported entries in old metadata are skipped.
//! - Namespaces reach the model only through `OpenAI` Responses; a thread on another provider
//!   fails its turn with the framework's refusal, as the reference's other backends refuse them.
//! - The tools are given to the thread's own agent, not to the agents it hands off to, and are
//!   registered after its declared and run-bound capability tools, skipping names already held
//!   by the host as Codex's external-tool registry does.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, PoisonError},
    time::Instant,
};

use async_trait::async_trait;
use ra_core::{
    error::{Error, Result},
    event::{HostEvent, HostEventBody, HostEventEmitter},
    item::{ImageBlock, ImageDetail, ImageSource},
    tool::{Tool, ToolContext, ToolNamespace, ToolOrigin, ToolOutput, ToolOutputBlock, ToolSchema},
};
use ra_runtime::thread::AgentThreadBuilder;
use ra_session::rollout::{RolloutPayload, RolloutRecord};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::oneshot;

use super::{
    answers::dynamic_tool_call_response,
    json_schema::parse_tool_input_schema,
    outgoing::ThreadScopedOutgoingMessageSender,
    protocol::{
        DynamicToolCallItem, DynamicToolCallOutputContentItem, DynamicToolCallParams,
        DynamicToolCallResponse, DynamicToolCallStatus, DynamicToolFunctionSpec,
        DynamicToolNamespaceSpec, DynamicToolNamespaceTool, DynamicToolSpec, ServerRequestPayload,
        ThreadItem, normalize_dynamic_tool_specs,
    },
    tool_items::ThreadToolItemEvent,
};

/// The host event family a call's request is recorded under: Codex's `DynamicToolCallRequest`.
const REQUEST_FAMILY: &str = "dynamicToolCallRequest";
/// The host event family a call's end is recorded under: Codex's `DynamicToolCallResponse`.
const RESPONSE_FAMILY: &str = "dynamicToolCallResponse";
/// What the model and the item are told of a call that ended without an answer: Codex's text.
const CANCELLED: &str = "dynamic tool call was cancelled before receiving a response";
/// The names Codex keeps from external tools whether or not a tool of the name is registered.
const RESERVED_NAMES: [&str; 2] = ["exec_command", "shell_command"];
/// What a result carrying audio is refused with, until tool results can carry it.
const AUDIO_UNSUPPORTED: &str = "dynamic tool audio output is not supported";

/// The calls of a thread's dynamic tools waiting for the client's answer, by call id: Codex's
/// `pending_dynamic_tools`.
#[derive(Debug, Default)]
pub(crate) struct DynamicToolCalls {
    pending: Mutex<HashMap<String, oneshot::Sender<DynamicToolCallResponse>>>,
}

impl DynamicToolCalls {
    fn insert(&self, call_id: &str) -> oneshot::Receiver<DynamicToolCallResponse> {
        let (sender, receiver) = oneshot::channel();
        let previous = self
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(call_id.to_owned(), sender);
        if previous.is_some() {
            tracing::warn!("Overwriting existing pending dynamic tool call for call_id: {call_id}");
        }
        receiver
    }

    fn remove(&self, call_id: &str) -> Option<oneshot::Sender<DynamicToolCallResponse>> {
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(call_id)
    }

    /// Hands `response` to the call `call_id` waits in: Codex's `notify_dynamic_tool_response`.
    fn deliver(&self, call_id: &str, response: DynamicToolCallResponse) {
        if let Some(sender) = self.remove(call_id) {
            let _ = sender.send(response);
        } else {
            tracing::warn!("No pending dynamic tool call found for call_id: {call_id}");
        }
    }
}

/// A call of a dynamic tool, as its request is recorded: Codex's `DynamicToolCallRequest`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DynamicToolCallRequestEvent {
    call_id: String,
    turn_id: String,
    #[serde(default)]
    started_at_ms: i64,
    #[serde(default)]
    namespace: Option<String>,
    tool: String,
    arguments: Value,
}

/// How a call of a dynamic tool ended, as it is recorded: Codex's `DynamicToolCallResponseEvent`,
/// with its duration in milliseconds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DynamicToolCallResponseEvent {
    call_id: String,
    turn_id: String,
    #[serde(default)]
    completed_at_ms: i64,
    #[serde(default)]
    namespace: Option<String>,
    tool: String,
    arguments: Value,
    content_items: Vec<DynamicToolCallOutputContentItem>,
    success: bool,
    #[serde(default)]
    error: Option<String>,
    duration_ms: i64,
}

fn event_data<T: for<'de> Deserialize<'de>>(event: &HostEvent, family: &str) -> Option<T> {
    match event.body() {
        HostEventBody::Unknown {
            family: recorded,
            data,
        } if recorded == family => serde_json::from_value(data.clone())
            .inspect_err(|error| tracing::warn!(%error, "skipping an unreadable {family} event"))
            .ok(),
        _ => None,
    }
}

fn emit(emitter: &HostEventEmitter, family: &str, data: &impl Serialize) -> Result<()> {
    let data = serde_json::to_value(data)
        .map_err(|error| Error::caller(format!("failed to record {family}: {error}")))?;
    emitter.emit(HostEventBody::Unknown {
        family: family.to_owned(),
        data,
    })?;
    Ok(())
}

/// The item a dynamic tool call's recorded event gives, live and when the history is read back:
/// Codex's `handle_dynamic_tool_call_request` and `handle_dynamic_tool_call_response`.
pub(crate) fn dynamic_tool_item(event: &HostEvent) -> Option<ThreadToolItemEvent> {
    if let Some(request) = event_data::<DynamicToolCallRequestEvent>(event, REQUEST_FAMILY) {
        let item = DynamicToolCallItem::new(
            request.call_id,
            request.tool,
            request.arguments,
            DynamicToolCallStatus::InProgress,
        );
        let item = match request.namespace {
            Some(namespace) => item.with_namespace(namespace),
            None => item,
        };
        return Some(ThreadToolItemEvent::Started(ThreadItem::from(item)));
    }
    let response = event_data::<DynamicToolCallResponseEvent>(event, RESPONSE_FAMILY)?;
    let status = if response.success {
        DynamicToolCallStatus::Completed
    } else {
        DynamicToolCallStatus::Failed
    };
    let item =
        DynamicToolCallItem::new(response.call_id, response.tool, response.arguments, status)
            .with_response(response.content_items, response.success)
            .with_duration_ms(response.duration_ms);
    let item = match response.namespace {
        Some(namespace) => item.with_namespace(namespace),
        None => item,
    };
    Some(ThreadToolItemEvent::Completed(ThreadItem::from(item)))
}

/// Asks the client to run the call `event` records, once its item has been sent, and hands the
/// answer to the call: the `DynamicToolCall` arm of Codex's `ItemStarted` handling and its
/// `dynamic_tools::on_call_response`. Does nothing for any other event.
///
/// As in Codex, the request is not reported resolved once answered.
pub(crate) async fn request_dynamic_tool_call(
    event: &HostEvent,
    calls: &Arc<DynamicToolCalls>,
    thread_id: &str,
    outgoing: &ThreadScopedOutgoingMessageSender,
) {
    let Some(request) = event_data::<DynamicToolCallRequestEvent>(event, REQUEST_FAMILY) else {
        return;
    };
    let params = DynamicToolCallParams::new(
        thread_id,
        request.turn_id,
        request.call_id.clone(),
        request.tool,
        request.arguments,
    );
    let params = match request.namespace {
        Some(namespace) => params.with_namespace(namespace),
        None => params,
    };
    let (_request_id, answer) = outgoing
        .send_request(ServerRequestPayload::DynamicToolCall(params))
        .await;
    let calls = Arc::clone(calls);
    tokio::spawn(async move {
        let Some(response) = dynamic_tool_call_response(answer.await) else {
            return;
        };
        calls.deliver(&request.call_id, refuse_audio(response));
    });
}

/// A result carrying audio, refused until tool results can carry it.
fn refuse_audio(response: DynamicToolCallResponse) -> DynamicToolCallResponse {
    if response
        .content_items()
        .iter()
        .any(|item| matches!(item, DynamicToolCallOutputContentItem::InputAudio { .. }))
    {
        tracing::error!(
            message = AUDIO_UNSUPPORTED,
            "dynamic tool response was invalid"
        );
        return DynamicToolCallResponse::new(
            [DynamicToolCallOutputContentItem::input_text(
                AUDIO_UNSUPPORTED,
            )],
            false,
        );
    }
    response
}

/// A function a client provides, as one of its thread's tools: Codex's `DynamicToolHandler`.
struct DynamicTool {
    origin: ToolOrigin,
    schema: ToolSchema,
    calls: Arc<DynamicToolCalls>,
}

impl DynamicTool {
    /// The tool for `function`, in `namespace` if one is given, or `None` if it cannot be
    /// described to the model: Codex's `DynamicToolHandler::from_parts`, which refuses a schema it
    /// cannot read. A namespace with no description is described as Codex describes it.
    fn new(
        function: &DynamicToolFunctionSpec,
        namespace: Option<&DynamicToolNamespaceSpec>,
        calls: &Arc<DynamicToolCalls>,
    ) -> Option<Self> {
        let name = function.name();
        let parameters = parse_tool_input_schema(function.input_schema())
            .and_then(serde_json::to_value)
            .ok()?;
        let mut schema = ToolSchema::loose(name, parameters)
            .ok()?
            .with_description(function.description());
        let origin = match namespace {
            Some(namespace) => {
                schema = schema.with_namespace_description(
                    if namespace.description().trim().is_empty() {
                        default_namespace_description(namespace.name())
                    } else {
                        namespace.description().to_owned()
                    },
                );
                ToolOrigin::namespaced(ToolNamespace::new(namespace.name()).ok()?, name).ok()?
            }
            None => ToolOrigin::new(name).ok()?,
        };
        Some(Self {
            origin,
            schema,
            calls: Arc::clone(calls),
        })
    }
}

/// Codex's `default_namespace_description`.
fn default_namespace_description(namespace: &str) -> String {
    format!("Tools in the {namespace} namespace.")
}

#[async_trait]
impl Tool for DynamicTool {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    async fn call(&self, context: ToolContext<'_>) -> Result<ToolOutput> {
        let emitter = context.event_emitter().ok_or_else(|| {
            Error::caller(format!(
                "dynamic tool `{}` was called outside a turn of its thread",
                self.origin.name()
            ))
        })?;
        let request = DynamicToolCallRequestEvent {
            call_id: context.call_id().as_str().to_owned(),
            turn_id: context.run().run_id().as_str().to_owned(),
            started_at_ms: super::outgoing::now_unix_timestamp_ms(),
            namespace: self
                .origin
                .namespace()
                .map(|namespace| namespace.as_str().to_owned()),
            tool: self.origin.name().to_owned(),
            arguments: context.arguments().clone(),
        };
        let answer = self.calls.insert(&request.call_id);
        let mut call = PendingCall {
            calls: &self.calls,
            emitter,
            request,
            started: Instant::now(),
            ended: false,
        };
        if let Err(error) = emit(&call.emitter, REQUEST_FAMILY, &call.request) {
            call.ended = true;
            self.calls.remove(&call.request.call_id);
            return Err(error);
        }
        let response = answer.await.ok();
        call.end(response.as_ref());
        Ok(match response {
            Some(response) => model_output(&response),
            None => ToolOutput::text(CANCELLED),
        })
    }
}

/// A call waiting for its answer. One dropped before it ends — its turn interrupted — ends failed,
/// as Codex's handler ends a call whose answer will not come.
struct PendingCall<'a> {
    calls: &'a DynamicToolCalls,
    emitter: HostEventEmitter,
    request: DynamicToolCallRequestEvent,
    started: Instant,
    ended: bool,
}

impl PendingCall<'_> {
    /// Records how the call ended: with the client's `response`, or without one.
    fn end(&mut self, response: Option<&DynamicToolCallResponse>) {
        self.ended = true;
        let (content_items, success, error) = match response {
            Some(response) => (response.content_items().to_vec(), response.success(), None),
            None => (Vec::new(), false, Some(CANCELLED.to_owned())),
        };
        let request = &self.request;
        let event = DynamicToolCallResponseEvent {
            call_id: request.call_id.clone(),
            turn_id: request.turn_id.clone(),
            completed_at_ms: super::outgoing::now_unix_timestamp_ms(),
            namespace: request.namespace.clone(),
            tool: request.tool.clone(),
            arguments: request.arguments.clone(),
            content_items,
            success,
            error,
            duration_ms: i64::try_from(self.started.elapsed().as_millis()).unwrap_or(i64::MAX),
        };
        if let Err(error) = emit(&self.emitter, RESPONSE_FAMILY, &event) {
            tracing::warn!(%error, "the end of a dynamic tool call could not be recorded");
        }
    }
}

impl Drop for PendingCall<'_> {
    fn drop(&mut self) {
        if !self.ended {
            self.calls.remove(&self.request.call_id);
            self.end(None);
        }
    }
}

/// What the model is given of the client's answer: Codex's conversion of its content items, an
/// image at Codex's default detail.
fn model_output(response: &DynamicToolCallResponse) -> ToolOutput {
    let blocks: Vec<ToolOutputBlock> = response
        .content_items()
        .iter()
        .filter_map(|item| match item {
            DynamicToolCallOutputContentItem::InputText { text, .. } => {
                Some(ToolOutputBlock::text(text.clone()))
            }
            DynamicToolCallOutputContentItem::InputImage { image_url, .. } => {
                Some(ToolOutputBlock::Image(
                    ImageBlock::new(ImageSource::url(image_url.clone()))
                        .with_detail(ImageDetail::High),
                ))
            }
            // Refused before it reaches the call.
            DynamicToolCallOutputContentItem::InputAudio { .. } => None,
        })
        .collect();
    ToolOutput::new(blocks).unwrap_or_else(|_| ToolOutput::text(""))
}

/// Gives the agent `builder` prepares the functions `specs` provide, in their namespaces, as
/// external tools — registered after the agent's own, skipping a name already held, as Codex's
/// registry does (see [`AgentBinding::with_external_tools`]) — and returns where their calls wait
/// for answers. A top-level function named like a tool Codex reserves is skipped first, and so is
/// a deferred function, until deferred tools are carried.
///
/// # Errors
///
/// Returns the error rebuilding the agent with its new tools.
pub(crate) fn register_dynamic_tools(
    builder: AgentThreadBuilder,
    specs: &[DynamicToolSpec],
) -> Result<(AgentThreadBuilder, Arc<DynamicToolCalls>)> {
    let calls = Arc::new(DynamicToolCalls::default());
    let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
    for spec in specs {
        let (namespace, functions): (_, Vec<&DynamicToolFunctionSpec>) = match spec {
            DynamicToolSpec::Function(function) => (None, vec![function]),
            DynamicToolSpec::Namespace(namespace) => (
                Some(namespace),
                namespace
                    .tools()
                    .iter()
                    .map(DynamicToolNamespaceTool::function)
                    .collect(),
            ),
        };
        for function in functions {
            let name = function.name();
            if function.defer_loading() {
                tracing::warn!(tool_name = name, "skipping a deferred dynamic tool");
                continue;
            }
            if namespace.is_none() && RESERVED_NAMES.contains(&name) {
                tracing::warn!(
                    tool_name = name,
                    "skipping external tool with reserved name"
                );
                continue;
            }
            let Some(tool) = DynamicTool::new(function, namespace, &calls) else {
                if let Some(namespace) = namespace {
                    tracing::error!(
                        "Failed to convert dynamic tool {:?}.{name:?} to OpenAI tool",
                        namespace.name()
                    );
                } else {
                    tracing::error!("Failed to convert dynamic tool {name:?} to OpenAI tool");
                }
                continue;
            };
            tools.push(Arc::new(tool));
        }
    }
    if tools.is_empty() {
        return Ok((builder, calls));
    }
    let binding = builder.agent().clone().with_external_tools(tools)?;
    Ok((builder.with_agent(binding), calls))
}

/// The dynamic tools the first session metadata of `records` that has any records: Codex's
/// `InitialHistory::get_dynamic_tools`.
pub(crate) fn dynamic_tools_from_records(records: &[RolloutRecord]) -> Vec<DynamicToolSpec> {
    records
        .iter()
        .find_map(|record| match record.payload() {
            Ok(RolloutPayload::SessionMeta(meta)) => meta.dynamic_tools().cloned(),
            _ => None,
        })
        .and_then(|tools| {
            let Value::Array(values) = tools else {
                tracing::warn!("skipping recorded dynamic tools that are not a list");
                return None;
            };
            normalize_dynamic_tool_specs(values)
                .inspect_err(|error| tracing::warn!(%error, "skipping unreadable dynamic tools"))
                .ok()
        })
        .unwrap_or_default()
}

/// `specs` as the session metadata records them, or `None` for none: Codex records an empty list
/// as absent.
pub(crate) fn recorded_dynamic_tools(specs: &[DynamicToolSpec]) -> Option<Value> {
    if specs.is_empty() {
        return None;
    }
    serde_json::to_value(specs)
        .inspect_err(|error| tracing::error!(%error, "failed to record dynamic tools"))
        .ok()
}
