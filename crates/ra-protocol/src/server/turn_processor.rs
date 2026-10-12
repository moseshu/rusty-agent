//! The turn methods: Codex's `TurnRequestProcessor` (`request_processors/turn_processor.rs`) for
//! `turn/start`, `turn/steer`, `turn/interrupt` and `thread/settings/update`.
//!
//! Each method looks its thread up among the loaded ones and hands the thread what the client
//! sent; refusals use Codex's wording. `turn/start` starts a turn when the thread is idle and
//! otherwise steers the running one, answering with the turn either way. `turn/steer` only steers,
//! and only the turn the client names. `turn/interrupt` is answered once the turn has ended, from
//! the thread's listener (see [`super::thread_listener`]), unless it names no turn, which
//! interrupts the thread's startup and is answered at once.
//!
//! # Differences from Codex
//!
//! Codex also checks that the thread takes input from a client rather than from its parent agent,
//! that its model provider is still configured, and records the client's name on the thread for
//! its requests to its backend; threads here are loaded by their host and none of that applies. Fields
//! that set what a run here does not have are refused (see [`TurnStartParams::unsupported_field`]).
//! A reasoning effort must be one a run here takes; Codex passes any name through to its model.
//! An image asking for `original` detail is refused, since a run here has no such detail, as are
//! audio, skills and mentions.

use std::sync::Arc;

use ra_core::{
    item::{ContentBlock, ImageBlock, ImageDetail as CoreImageDetail, ImageSource},
    model::{Effort, ResolvedModel},
    session::SessionId,
};
use ra_runtime::thread::{
    AgentThread, AgentThreadBuilder, NotSubmittedReason, SteerSubmission, ThreadSettingsOverrides,
    TurnInputRequest, TurnInputSubmission, TurnSettingsUpdateOutcome, TurnStartOptions,
};
use serde_json::Value;

use super::{
    image_url::{REMOTE_IMAGE_URL_ERROR, is_remote_image_url},
    outgoing::{ConnectionRequestId, OutgoingMessageSender},
    protocol::{
        ApprovalsReviewer, ImageDetail, ImageReference, ReasoningSummary, ThreadPolicyOverrides,
        ThreadSettingsUpdateParams, ThreadSettingsUpdateResponse, Turn, TurnInterruptParams,
        TurnInterruptResponse, TurnSettingsUpdateParams, TurnSettingsUpdateResponse,
        TurnSettingsUpdateStatus, TurnStartParams, TurnStartResponse, TurnStatus, TurnSteerParams,
        TurnSteerResponse, UserInput,
    },
    settings_adapter::ThreadSettingsAdapter,
    thread_manager::{LoadedThread, ThreadManager},
};
use crate::frame::JsonRpcErrorObject;

/// The most characters of text one request may carry: Codex's `MAX_USER_INPUT_TEXT_CHARS`.
pub(crate) const MAX_USER_INPUT_TEXT_CHARS: usize = 1 << 20;

/// The code in the data of the error that refuses too much text: Codex's
/// `INPUT_TOO_LARGE_ERROR_CODE`.
pub(crate) const INPUT_TOO_LARGE_ERROR_CODE: &str = "input_too_large";

/// What Codex's submission to a thread whose session loop has ended fails with
/// (`CodexErr::InternalAgentDied`): a thread that was shut down while the request was on its way.
const AGENT_LOOP_DIED: &str = "internal error; agent loop died unexpectedly";

/// What a turn method answers: a response now, nothing yet, or an error.
pub(crate) type TurnMethodResult = Result<Option<Value>, JsonRpcErrorObject>;

/// Handles the turn methods: Codex's `TurnRequestProcessor`.
#[derive(Debug)]
pub(crate) struct TurnRequestProcessor {
    thread_manager: Arc<ThreadManager>,
    outgoing: Arc<OutgoingMessageSender>,
}

impl TurnRequestProcessor {
    pub(crate) const fn new(
        thread_manager: Arc<ThreadManager>,
        outgoing: Arc<OutgoingMessageSender>,
    ) -> Self {
        Self {
            thread_manager,
            outgoing,
        }
    }

    /// `turn/start`: Codex's `turn_start`.
    pub(crate) async fn turn_start(&self, params: TurnStartParams) -> TurnMethodResult {
        validate_user_input_image_urls(params.input())?;
        let thread = self.load_thread(params.thread_id())?;
        let _settings_guard = thread.settings_lock.lock().await;
        if let Some(field) = params.unsupported_field() {
            return Err(unsupported("turn/start", field));
        }
        validate_input_limit(params.input())?;
        let policy = params.policy().clone();
        let summary = params.summary();
        let tier = params.service_tier().map(|tier| tier.map(str::to_owned));
        let (input, model, effort, output_schema) = params.into_parts();
        let content = input_content(input)?;
        let overrides = prepare_overrides(
            "turn/start",
            &SettingsTarget::of(&thread),
            thread.run_config(),
            model,
            effort,
            &policy,
            summary,
            tier,
        )?;
        let mut start = TurnStartOptions::new();
        if let Some(schema) = output_schema {
            start = start.with_final_output_json_schema(schema);
        }
        let request = TurnInputRequest::new(content)
            .with_thread_settings(overrides)
            .on_start(start);
        let turn_id = match thread.start_or_steer_turn(request).await {
            TurnInputSubmission::Started { turn_id } | TurnInputSubmission::Steered { turn_id } => {
                turn_id
            }
            TurnInputSubmission::NotSubmitted {
                reason: NotSubmittedReason::ShutDown,
            } => {
                return Err(JsonRpcErrorObject::internal_error(format!(
                    "failed to submit turn input: {AGENT_LOOP_DIED}"
                )));
            }
            TurnInputSubmission::NotSubmitted { reason } => {
                return Err(JsonRpcErrorObject::internal_error(format!(
                    "failed to submit turn input: {reason:?}"
                )));
            }
            submission => {
                return Err(JsonRpcErrorObject::internal_error(format!(
                    "failed to submit turn input: {submission:?}"
                )));
            }
        };
        response(TurnStartResponse::new(Turn::new(
            turn_id,
            TurnStatus::InProgress,
        )))
    }

    /// `turn/steer`: Codex's `turn_steer`.
    pub(crate) fn turn_steer(&self, params: TurnSteerParams) -> TurnMethodResult {
        validate_user_input_image_urls(params.input())?;
        let thread = self.load_thread(params.thread_id())?;
        if let Some(field) = params.unsupported_field() {
            return Err(unsupported("turn/steer", field));
        }
        if params.expected_turn_id().is_empty() {
            return Err(JsonRpcErrorObject::invalid_request(
                "expectedTurnId must not be empty",
            ));
        }
        validate_input_limit(params.input())?;
        let (_, input, expected_turn_id) = params.into_parts();
        let request = TurnInputRequest::new(input_content(input)?);
        match thread.steer_turn(&request, &expected_turn_id) {
            SteerSubmission::Steered { turn_id } => response(TurnSteerResponse::new(turn_id)),
            SteerSubmission::NotSubmitted {
                reason: NotSubmittedReason::ShutDown,
            } => Err(JsonRpcErrorObject::internal_error(format!(
                "failed to steer turn: {AGENT_LOOP_DIED}"
            ))),
            SteerSubmission::NotSubmitted { reason } => {
                Err(JsonRpcErrorObject::invalid_request(match reason {
                    NotSubmittedReason::ExpectedTurnMismatch { expected, actual } => {
                        format!("expected active turn id `{expected}` but found `{actual}`")
                    }
                    NotSubmittedReason::EmptyInput => "input must not be empty".to_owned(),
                    _ => "no active turn to steer".to_owned(),
                }))
            }
            submission => Err(JsonRpcErrorObject::internal_error(format!(
                "failed to steer turn: {submission:?}"
            ))),
        }
    }

    /// `turn/interrupt`: Codex's `turn_interrupt`. A turn's interrupt is answered when the turn
    /// ends, so this returns no response for it.
    pub(crate) async fn turn_interrupt(
        &self,
        request_id: &ConnectionRequestId,
        params: TurnInterruptParams,
    ) -> TurnMethodResult {
        let (thread_id, turn_id) = params.into_parts();
        let is_startup_interrupt = turn_id.is_empty();
        let thread = self.load_thread(&thread_id)?;

        // Recorded so the end of the turn answers it. A startup interrupt has no turn to wait for.
        if !is_startup_interrupt {
            let thread_state = self
                .outgoing
                .thread_state()
                .thread_state(thread.thread_id());
            let is_running = thread.is_running();
            let mut thread_state = thread_state.lock().await;
            if let Some(active_turn_id) = thread_state.active_turn_id() {
                if active_turn_id != turn_id {
                    return Err(JsonRpcErrorObject::invalid_request(format!(
                        "expected active turn id {turn_id} but found {active_turn_id}"
                    )));
                }
            } else if thread_state.last_terminal_turn_id.as_deref() == Some(turn_id.as_str())
                || !is_running
            {
                return Err(JsonRpcErrorObject::invalid_request(
                    "no active turn to interrupt",
                ));
            }
            thread_state.pending_interrupts.push(request_id.clone());
        }

        // Whether a turn was running or not, the answer comes from the turn's end: the listener
        // has yet to hear of a turn that ended before this.
        let _ = thread.interrupt();
        if is_startup_interrupt {
            response(TurnInterruptResponse::new())
        } else {
            Ok(None)
        }
    }

    /// `thread/settings/update`: Codex's `thread_settings_update`.
    pub(crate) async fn thread_settings_update(
        &self,
        params: ThreadSettingsUpdateParams,
    ) -> TurnMethodResult {
        let thread = self.load_thread(params.thread_id())?;
        let _settings_guard = thread.settings_lock.lock().await;
        if let Some(field) = params.unsupported_field() {
            return Err(unsupported("thread/settings/update", field));
        }
        let policy = params.policy().clone();
        let summary = params.summary();
        let tier = params.service_tier().map(|tier| tier.map(str::to_owned));
        let (_, model, effort) = params.into_parts();
        let overrides = prepare_overrides(
            "thread/settings/update",
            &SettingsTarget::of(&thread),
            thread.run_config(),
            model,
            effort,
            &policy,
            summary,
            tier,
        )?;
        if !overrides.is_empty() {
            thread.update_settings(&overrides);
        }
        response(ThreadSettingsUpdateResponse::new())
    }

    /// Publishes changes for subsequent captures of the named turn only.
    pub(crate) async fn turn_settings_update(
        &self,
        params: TurnSettingsUpdateParams,
    ) -> TurnMethodResult {
        let thread = self.load_thread(&params.thread_id)?;
        let _settings_guard = thread.settings_lock.lock().await;
        check_approvals_reviewer("turn/settings/update", params.approvals_reviewer)?;
        let requires_model_switching = params.model.is_some()
            || params.effort.is_some()
            || params.summary.is_some()
            || params.service_tier.is_some()
            || params.approvals_reviewer.is_none();
        if requires_model_switching && !thread.step_model_switching_enabled() {
            return Err(JsonRpcErrorObject::invalid_request(
                "turn settings updates require the step_model_switching feature",
            ));
        }
        let Some(config) = thread.turn_config(&params.turn_id) else {
            let mut overrides =
                thread_settings_overrides("turn/settings/update", params.model, params.effort)?;
            if params.summary.is_some() || params.service_tier.is_some() {
                overrides = overrides.with_model_settings(ra_core::model::ModelSettings::new());
            }
            if params.approvals_reviewer.is_some() {
                overrides = overrides.with_config(thread.run_config());
            }
            return turn_settings_response(
                thread.update_turn_settings(&params.turn_id, &overrides),
            );
        };
        let policy = ThreadPolicyOverrides {
            approvals_reviewer: params.approvals_reviewer,
            ..ThreadPolicyOverrides::default()
        };
        let overrides = prepare_overrides(
            "turn/settings/update",
            &SettingsTarget::of(&thread),
            config,
            params.model,
            params.effort,
            &policy,
            params.summary,
            params.service_tier,
        )?;
        turn_settings_response(thread.update_turn_settings(&params.turn_id, &overrides))
    }

    /// The loaded thread `thread_id`: Codex's `load_thread`.
    fn load_thread(&self, thread_id: &str) -> Result<LoadedThread, JsonRpcErrorObject> {
        self.thread_manager
            .get_thread(&SessionId::new(thread_id))
            .ok_or_else(|| {
                JsonRpcErrorObject::invalid_request(format!("thread not found: {thread_id}"))
            })
    }
}

fn response(response: impl serde::Serialize) -> TurnMethodResult {
    serde_json::to_value(response).map(Some).map_err(|error| {
        JsonRpcErrorObject::internal_error(format!("failed to serialize response: {error}"))
    })
}

fn turn_settings_response(outcome: TurnSettingsUpdateOutcome) -> TurnMethodResult {
    let status = match outcome {
        TurnSettingsUpdateOutcome::Applied => TurnSettingsUpdateStatus::Applied,
        TurnSettingsUpdateOutcome::TargetUnavailable => TurnSettingsUpdateStatus::TargetUnavailable,
        TurnSettingsUpdateOutcome::Rejected { reason } => {
            return Err(JsonRpcErrorObject::invalid_request(reason));
        }
        outcome => {
            return Err(JsonRpcErrorObject::internal_error(format!(
                "failed to update turn settings: {outcome:?}"
            )));
        }
    };
    response(TurnSettingsUpdateResponse { status })
}

/// Codex previews a complete candidate before admission. Product fields are lowered by the host
/// adapter; provider-only fields use the already existing model-settings extras channel.
#[allow(
    clippy::too_many_arguments,
    clippy::option_option,
    reason = "Codex distinguishes omitted, null and supplied service tiers"
)]
pub(crate) fn prepare_overrides(
    method: &str,
    target: &SettingsTarget<'_>,
    mut config: ra_runtime::runner::RunConfig,
    model: Option<String>,
    effort: Option<String>,
    policy: &ThreadPolicyOverrides,
    summary: Option<ReasoningSummary>,
    service_tier: Option<Option<String>>,
) -> Result<ThreadSettingsOverrides, JsonRpcErrorObject> {
    policy
        .validate()
        .map_err(JsonRpcErrorObject::invalid_request)?;
    check_approvals_reviewer(method, policy.approvals_reviewer)?;
    if let Some(model) = model.as_deref() {
        target
            .resolve(Some(model))
            .map_err(|error| JsonRpcErrorObject::invalid_request(error.to_string()))?;
    }
    let target_provider = if summary.is_some() || service_tier.is_some() {
        Some(
            target
                .resolve(model.as_deref().or(config.model()))
                .map_err(|error| JsonRpcErrorObject::invalid_request(error.to_string()))?
                .selector()
                .provider()
                .clone(),
        )
    } else {
        None
    };
    let mut overrides = thread_settings_overrides(method, model, effort)?;
    let changes_config = !policy.is_empty() || summary.is_some() || service_tier.is_some();
    if !policy.is_empty() {
        let adapter = target.adapter.ok_or_else(|| {
            JsonRpcErrorObject::invalid_request(
                "the thread's host has not configured a permission settings adapter",
            )
        })?;
        config = adapter.prepare(&config, policy).map_err(|reason| {
            JsonRpcErrorObject::invalid_request(format!(
                "invalid thread settings override: {reason}"
            ))
        })?;
    }
    if summary.is_some() || service_tier.is_some() {
        let provider = target_provider
            .ok_or_else(|| JsonRpcErrorObject::internal_error("missing target provider"))?;
        let mut settings = config.model_settings().clone();
        if let Some(summary) = summary {
            let mut reasoning = settings
                .extra_body()
                .get(&provider)
                .and_then(|body| body.get("reasoning"))
                .cloned()
                .unwrap_or_else(|| serde_json::json!({}));
            let object = reasoning.as_object_mut().ok_or_else(|| {
                JsonRpcErrorObject::invalid_request(
                    "the model's reasoning settings must be an object",
                )
            })?;
            object.insert(
                "summary".into(),
                serde_json::to_value(summary)
                    .map_err(|error| JsonRpcErrorObject::internal_error(error.to_string()))?,
            );
            settings = settings.with_extra_body_value(provider.clone(), "reasoning", reasoning);
        }
        if let Some(tier) = service_tier {
            settings = settings.with_extra_body_value(
                provider,
                "service_tier",
                tier.map_or(Value::Null, Value::String),
            );
        }
        overrides = overrides.with_model_settings(settings);
    }
    if changes_config {
        overrides = overrides.with_config(config);
    }
    Ok(overrides)
}

/// What settings are prepared against: how the thread resolves models, and the host adapter its
/// product settings go through, for a loaded thread or for one about to be built.
pub(crate) struct SettingsTarget<'a> {
    pub(crate) models: &'a dyn ResolveModel,
    pub(crate) adapter: Option<&'a dyn ThreadSettingsAdapter>,
}

impl<'a> SettingsTarget<'a> {
    fn of(thread: &'a LoadedThread) -> Self {
        Self {
            models: &thread.thread,
            adapter: thread.adapter.as_deref(),
        }
    }

    fn resolve(&self, model: Option<&str>) -> ra_core::error::Result<ResolvedModel> {
        self.models.resolve(model)
    }
}

/// Resolves models as a thread does, built or not.
pub(crate) trait ResolveModel {
    fn resolve(&self, model: Option<&str>) -> ra_core::error::Result<ResolvedModel>;
}

impl ResolveModel for AgentThread {
    fn resolve(&self, model: Option<&str>) -> ra_core::error::Result<ResolvedModel> {
        self.resolve_model(model)
    }
}

impl ResolveModel for AgentThreadBuilder {
    fn resolve(&self, model: Option<&str>) -> ra_core::error::Result<ResolvedModel> {
        self.resolve_model(model)
    }
}

/// Refuses `auto_review`: Codex routes those approvals to its Guardian reviewer, which is not
/// ported, so the client is the only reviewer a thread here has.
pub(crate) fn check_approvals_reviewer(
    method: &str,
    reviewer: Option<ApprovalsReviewer>,
) -> Result<(), JsonRpcErrorObject> {
    if reviewer == Some(ApprovalsReviewer::AutoReview) {
        return Err(JsonRpcErrorObject::invalid_request(format!(
            "{method}.approvalsReviewer `auto_review` is not supported"
        )));
    }
    Ok(())
}

/// The refusal of a field this port does not carry.
pub(crate) fn unsupported(method: &str, field: &str) -> JsonRpcErrorObject {
    JsonRpcErrorObject::invalid_request(format!("{method}.{field} is not supported"))
}

/// Refuses an image the provider would fetch over the network: Codex's
/// `validate_user_input_image_urls`.
fn validate_user_input_image_urls(input: &[UserInput]) -> Result<(), JsonRpcErrorObject> {
    let remote = input.iter().any(|item| {
        matches!(
            item,
            UserInput::Image {
                image: ImageReference::Inline { url },
                ..
            } if is_remote_image_url(url)
        )
    });
    if remote {
        return Err(JsonRpcErrorObject::invalid_request(REMOTE_IMAGE_URL_ERROR));
    }
    Ok(())
}

/// Refuses more text than one request may carry: Codex's `validate_v2_input_limit`.
fn validate_input_limit(input: &[UserInput]) -> Result<(), JsonRpcErrorObject> {
    let actual_chars: usize = input.iter().map(UserInput::text_char_count).sum();
    if actual_chars > MAX_USER_INPUT_TEXT_CHARS {
        return Err(input_too_large_error(actual_chars));
    }
    Ok(())
}

/// Codex's `input_too_large_error`.
fn input_too_large_error(actual_chars: usize) -> JsonRpcErrorObject {
    JsonRpcErrorObject::invalid_params(format!(
        "Input exceeds the maximum length of {MAX_USER_INPUT_TEXT_CHARS} characters."
    ))
    .with_data(serde_json::json!({
        "input_error_code": INPUT_TOO_LARGE_ERROR_CODE,
        "max_chars": MAX_USER_INPUT_TEXT_CHARS,
        "actual_chars": actual_chars,
    }))
}

/// The user input as the content of one user message, as Codex's `into_core` turns it into the
/// model's input.
fn input_content(input: Vec<UserInput>) -> Result<Vec<ContentBlock>, JsonRpcErrorObject> {
    input
        .into_iter()
        .map(|item| match item {
            UserInput::Text { text, .. } => Ok(ContentBlock::text(text)),
            UserInput::Image { image, detail } => {
                let source = match image {
                    ImageReference::Inline { url } => ImageSource::url(url),
                    ImageReference::File { file_id } => ImageSource::provider_file(file_id),
                };
                image_content(source, detail)
            }
            UserInput::LocalImage { detail, path } => {
                image_content(ImageSource::local_path(path), detail)
            }
            UserInput::Audio { .. } | UserInput::LocalAudio { .. } => Err(
                JsonRpcErrorObject::invalid_request("audio input is not supported"),
            ),
            UserInput::Skill { .. } => Err(JsonRpcErrorObject::invalid_request(
                "skill input is not supported",
            )),
            UserInput::Mention { .. } => Err(JsonRpcErrorObject::invalid_request(
                "mention input is not supported",
            )),
        })
        .collect()
}

fn image_content(
    source: ImageSource,
    detail: Option<ImageDetail>,
) -> Result<ContentBlock, JsonRpcErrorObject> {
    let block = ImageBlock::new(source);
    let block = match detail {
        None => block,
        Some(ImageDetail::Auto) => block.with_detail(CoreImageDetail::Auto),
        Some(ImageDetail::Low) => block.with_detail(CoreImageDetail::Low),
        Some(ImageDetail::High) => block.with_detail(CoreImageDetail::High),
        Some(ImageDetail::Original) => {
            return Err(JsonRpcErrorObject::invalid_request(
                "image detail `original` is not supported",
            ));
        }
    };
    Ok(ContentBlock::Image(block))
}

/// The thread settings a request asks for. A reasoning effort a run here does not take is
/// refused.
pub(crate) fn thread_settings_overrides(
    method: &str,
    model: Option<String>,
    effort: Option<String>,
) -> Result<ThreadSettingsOverrides, JsonRpcErrorObject> {
    let mut overrides = ThreadSettingsOverrides::new();
    if let Some(model) = model {
        overrides = overrides.with_model(model);
    }
    if let Some(effort) = effort {
        let effort = match effort.as_str() {
            "low" => Effort::Low,
            "medium" => Effort::Medium,
            "high" => Effort::High,
            "xhigh" => Effort::XHigh,
            "max" => Effort::Max,
            _ => {
                return Err(JsonRpcErrorObject::invalid_request(format!(
                    "{method}.effort `{effort}` is not supported"
                )));
            }
        };
        overrides = overrides.with_effort(effort);
    }
    Ok(overrides)
}
