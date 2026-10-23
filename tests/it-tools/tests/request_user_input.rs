//! `request_user_input`: Codex's tool for asking the user, ported with its schema, description,
//! mode availability, argument normalization and refusals (`request_user_input_spec_tests.rs`,
//! `request_user_input_tests.rs`, and the core suite's `request_user_input.rs`).

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ra_core::{
    agent::AgentSpec,
    context::RunContext,
    item::{AgentId, CallId},
    permission::ActionClass,
    state::RunId,
    tool::{Tool, ToolConcurrency, ToolContext, ToolOutput, ToolServices},
    user_input::{
        RequestUserInputAnswer, RequestUserInputArgs, RequestUserInputQuestion,
        RequestUserInputQuestionOption, RequestUserInputResponse, UserInputPort,
    },
};
use ra_tools::request_user_input::{
    ModeKind, RequestUserInputTool, request_user_input_available_modes,
    request_user_input_tool_description, request_user_input_unavailable_message,
};
use serde_json::{Value, json};

/// What a port was asked, and what it answers with.
struct ScriptedPort {
    answer: Option<RequestUserInputResponse>,
    asked: Mutex<Vec<(String, String, RequestUserInputArgs)>>,
}

impl ScriptedPort {
    fn answering(answer: Option<RequestUserInputResponse>) -> Arc<Self> {
        Arc::new(Self {
            answer,
            asked: Mutex::new(Vec::new()),
        })
    }

    fn asked(&self) -> Vec<(String, String, RequestUserInputArgs)> {
        self.asked.lock().expect("the port's record").clone()
    }
}

#[async_trait]
impl UserInputPort for ScriptedPort {
    async fn request_user_input(
        &self,
        run: &RunContext,
        call_id: &CallId,
        args: RequestUserInputArgs,
    ) -> Option<RequestUserInputResponse> {
        self.asked.lock().expect("the port's record").push((
            run.run_id().as_str().to_owned(),
            call_id.as_str().to_owned(),
            args,
        ));
        self.answer.clone()
    }
}

fn run() -> RunContext {
    let agent = AgentSpec::builder()
        .id(AgentId::new("asker"))
        .name("Asker")
        .build()
        .expect("an agent");
    RunContext::new(RunId::new("turn-1"), agent.as_ref())
}

fn default_mode_tool() -> RequestUserInputTool {
    RequestUserInputTool::new(request_user_input_available_modes(true)).expect("the tool builds")
}

fn confirm_arguments() -> Value {
    json!({
        "questions": [{
            "id": "confirm_path",
            "header": "Confirm",
            "question": "Proceed with the plan?",
            "options": [{
                "label": "Yes (Recommended)",
                "description": "Continue the current plan."
            }, {
                "label": "No",
                "description": "Stop and revisit the approach."
            }]
        }]
    })
}

/// Runs the call and, when it fails, the tool's own failure shaping — the path the dispatcher takes
/// for a tool declaring `ToolFailureHandling::Custom`.
async fn observe(
    tool: &RequestUserInputTool,
    run: &RunContext,
    services: &ToolServices,
    arguments: &Value,
) -> ToolOutput {
    let call_id = CallId::new("call-1");
    let context = || ToolContext::new(run, tool, &call_id, arguments).with_services(services);
    match tool.call(context()).await {
        Ok(output) => output,
        Err(error) => tool
            .handle_failure(&context(), &error)
            .await
            .expect("failure shaping must not fail")
            .expect("request_user_input shapes every refusal it produces"),
    }
}

fn body(output: &ToolOutput) -> &str {
    output.as_text().expect("a single text block")
}

fn services(port: &Arc<ScriptedPort>) -> ToolServices {
    ToolServices::new().with_user_input(Arc::clone(port) as Arc<dyn UserInputPort>)
}

/// Codex's `request_user_input_tool_includes_questions_schema`.
#[test]
fn request_user_input_tool_includes_questions_schema() {
    let tool = RequestUserInputTool::new(vec![ModeKind::Plan]).expect("the tool builds");

    tool.validate().expect("identity and schema must agree");
    assert_eq!(tool.origin().qualified_name(), "request_user_input");
    assert!(!tool.schema().strict_json_schema());
    assert_eq!(
        tool.schema().description(),
        Some(
            "Request user input for one to three short questions and wait for the response. This tool is only available in Plan mode."
        )
    );
    assert_eq!(
        tool.schema().input_schema(),
        &json!({
            "type": "object",
            "properties": {
                "questions": {
                    "type": "array",
                    "description": "Questions to show the user. Prefer 1 and do not exceed 3",
                    "items": {
                        "type": "object",
                        "properties": {
                            "header": {
                                "type": "string",
                                "description": "Short header label shown in the UI (12 or fewer chars).",
                            },
                            "id": {
                                "type": "string",
                                "description": "Stable identifier for mapping answers (snake_case).",
                            },
                            "options": {
                                "type": "array",
                                "description": "Provide 2-3 mutually exclusive choices. Put the recommended option first and suffix its label with \"(Recommended)\". Do not include an \"Other\" option in this list; the client will add a free-form \"Other\" option automatically.",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "description": {
                                            "type": "string",
                                            "description": "One short sentence explaining impact/tradeoff if selected.",
                                        },
                                        "label": {
                                            "type": "string",
                                            "description": "User-facing label (1-5 words).",
                                        },
                                    },
                                    "required": ["label", "description"],
                                    "additionalProperties": false,
                                },
                            },
                            "question": {
                                "type": "string",
                                "description": "Single-sentence prompt shown to the user.",
                            },
                        },
                        "required": ["id", "header", "question", "options"],
                        "additionalProperties": false,
                    },
                },
            },
            "required": ["questions"],
            "additionalProperties": false,
        })
    );
    // Codex's tools run alone unless they declare parallel calls, and asking reaches only the user.
    assert_eq!(tool.options().concurrency(), ToolConcurrency::Exclusive);
    assert_eq!(
        tool.options().action_classes(),
        &[ActionClass::from("read")]
    );
}

/// Codex's `request_user_input_unavailable_messages_respect_default_mode_feature_flag`.
#[test]
fn request_user_input_unavailable_messages_respect_default_mode_feature_flag() {
    let default_modes = request_user_input_available_modes(false);
    let enabled_modes = request_user_input_available_modes(true);
    assert_eq!(default_modes, vec![ModeKind::Plan]);
    assert_eq!(enabled_modes, vec![ModeKind::Default, ModeKind::Plan]);

    assert_eq!(
        request_user_input_unavailable_message(ModeKind::Plan, &default_modes),
        None
    );
    assert_eq!(
        request_user_input_unavailable_message(ModeKind::Default, &default_modes),
        Some("request_user_input is unavailable in Default mode".to_owned())
    );
    assert_eq!(
        request_user_input_unavailable_message(ModeKind::Default, &enabled_modes),
        None
    );
}

/// Codex's `request_user_input_tool_description_mentions_available_modes`, with the remaining
/// shapes of its mode list.
#[test]
fn request_user_input_tool_description_mentions_available_modes() {
    let prefix = "Request user input for one to three short questions and wait for the response. This tool is only available in";
    assert_eq!(
        request_user_input_tool_description(&request_user_input_available_modes(false)),
        format!("{prefix} Plan mode.")
    );
    assert_eq!(
        request_user_input_tool_description(&request_user_input_available_modes(true)),
        format!("{prefix} Default or Plan mode.")
    );
    assert_eq!(
        request_user_input_tool_description(&[ModeKind::Default]),
        format!("{prefix} Default mode.")
    );
    assert_eq!(
        request_user_input_tool_description(&[]),
        format!("{prefix} no modes.")
    );
    assert_eq!(
        request_user_input_tool_description(&[ModeKind::Default, ModeKind::Plan, ModeKind::Plan]),
        format!("{prefix} modes: Default,Plan,Plan.")
    );
}

/// Codex's `request_user_input_sets_non_blocking_outside_plan_mode`, and the core suite's round
/// trip in Default mode: the questions reach the port normalized, with the call and its turn, and
/// the model is given the answers as JSON.
#[tokio::test]
async fn request_user_input_round_trip_in_default_mode() {
    let port = ScriptedPort::answering(Some(
        RequestUserInputResponse::new()
            .with_answer("confirm_path", RequestUserInputAnswer::new(["yes"])),
    ));
    let output = observe(
        &default_mode_tool(),
        &run(),
        &services(&port),
        &confirm_arguments(),
    )
    .await;

    assert_eq!(
        serde_json::from_str::<Value>(body(&output)).expect("the answers as JSON"),
        json!({ "answers": { "confirm_path": { "answers": ["yes"] } } })
    );
    let asked = port.asked();
    assert_eq!(asked.len(), 1);
    let (turn_id, call_id, args) = &asked[0];
    assert_eq!(turn_id, "turn-1");
    assert_eq!(call_id, "call-1");
    assert!(!args.is_blocking());
    assert_eq!(args.auto_resolution_ms(), None);
    assert_eq!(
        args.questions(),
        [
            RequestUserInputQuestion::new("confirm_path", "Confirm", "Proceed with the plan?")
                .with_is_other(true)
                .with_options([
                    RequestUserInputQuestionOption::new(
                        "Yes (Recommended)",
                        "Continue the current plan."
                    ),
                    RequestUserInputQuestionOption::new("No", "Stop and revisit the approach."),
                ])
        ]
    );
}

/// Codex's request_user_input_sets_blocking_from_turn_mode.
#[tokio::test]
async fn request_user_input_marks_plan_mode_requests_blocking() {
    let port = ScriptedPort::answering(Some(RequestUserInputResponse::new()));
    let tool = RequestUserInputTool::new(request_user_input_available_modes(false))
        .unwrap()
        .with_mode(ModeKind::Plan);
    let output = observe(&tool, &run(), &services(&port), &confirm_arguments()).await;
    assert_eq!(
        serde_json::from_str::<Value>(body(&output)).unwrap(),
        json!({"answers":{}})
    );
    let asked = port.asked();
    assert_eq!(asked.len(), 1);
    assert!(asked[0].2.is_blocking());
    assert_eq!(asked[0].2.auto_resolution_ms(), None);
}

/// Codex's `normalize_request_user_input_tool_args_sets_other_on_every_question`: whatever the
/// model sends, every question lets the user answer otherwise; `isSecret` passes through.
#[tokio::test]
async fn every_question_lets_the_user_answer_otherwise() {
    let port = ScriptedPort::answering(Some(RequestUserInputResponse::new()));
    let arguments = json!({
        "questions": [{
            "id": "confirm",
            "header": "Confirm",
            "question": "Proceed?",
            "isOther": false,
            "options": [{ "label": "Yes (Recommended)", "description": "Continue." }]
        }, {
            "id": "token",
            "header": "Token",
            "question": "Which token?",
            "isSecret": true,
            "options": [{ "label": "Staging", "description": "The staging token." }]
        }]
    });
    let output = observe(&default_mode_tool(), &run(), &services(&port), &arguments).await;

    assert_eq!(body(&output), r#"{"answers":{}}"#);
    let questions = port.asked()[0].2.questions().to_vec();
    assert!(questions.iter().all(RequestUserInputQuestion::is_other));
    assert_eq!(
        questions
            .iter()
            .map(RequestUserInputQuestion::is_secret)
            .collect::<Vec<_>>(),
        [false, true]
    );
}

/// Codex's `normalize_request_user_input_tool_args_rejects_missing_options`, for missing and for
/// empty options; nothing is asked.
#[tokio::test]
async fn questions_without_options_are_refused() {
    for options in [None, Some(json!([]))] {
        let port = ScriptedPort::answering(Some(RequestUserInputResponse::new()));
        let mut question = json!({ "id": "confirm", "header": "Confirm", "question": "Proceed?" });
        if let Some(options) = options {
            question["options"] = options;
        }
        let output = observe(
            &default_mode_tool(),
            &run(),
            &services(&port),
            &json!({ "questions": [question] }),
        )
        .await;

        assert_eq!(
            body(&output),
            "request_user_input requires non-empty options for every question"
        );
        assert!(port.asked().is_empty());
    }
}

/// Codex's `multi_agent_v2_request_user_input_rejects_subagent_threads`: a run another run started
/// is no root thread.
#[tokio::test]
async fn request_user_input_rejects_runs_started_by_another_run() {
    let port = ScriptedPort::answering(Some(RequestUserInputResponse::new()));
    let nested = run().with_parent_run_id(RunId::new("parent-run"));
    let output = observe(
        &default_mode_tool(),
        &nested,
        &services(&port),
        &confirm_arguments(),
    )
    .await;

    assert_eq!(
        body(&output),
        "request_user_input can only be used by the root thread"
    );
    assert!(port.asked().is_empty());
}

/// The core suite's `request_user_input_rejected_in_default_mode_by_default`: without the host
/// enabling Default mode, the only mode a turn has here, the call is refused.
#[tokio::test]
async fn request_user_input_rejected_in_default_mode_by_default() {
    let port = ScriptedPort::answering(Some(RequestUserInputResponse::new()));
    let tool = RequestUserInputTool::new(request_user_input_available_modes(false))
        .expect("the tool builds");
    let output = observe(&tool, &run(), &services(&port), &confirm_arguments()).await;

    assert_eq!(
        body(&output),
        "request_user_input is unavailable in Default mode"
    );
    assert!(port.asked().is_empty());
}

/// Arguments that do not parse are refused with the parser's words, as Codex's `parse_arguments`
/// refuses them.
#[tokio::test]
async fn unparseable_arguments_are_refused() {
    let port = ScriptedPort::answering(Some(RequestUserInputResponse::new()));
    let output = observe(
        &default_mode_tool(),
        &run(),
        &services(&port),
        &json!({ "questions": "confirm" }),
    )
    .await;

    assert!(
        body(&output).starts_with("failed to parse function arguments: "),
        "{}",
        body(&output)
    );
    assert!(port.asked().is_empty());
}

/// A request that ends without answers ends the call with Codex's text.
#[tokio::test]
async fn a_request_ended_without_answers_is_cancelled() {
    let port = ScriptedPort::answering(None);
    let output = observe(
        &default_mode_tool(),
        &run(),
        &services(&port),
        &confirm_arguments(),
    )
    .await;

    assert_eq!(
        body(&output),
        "request_user_input was cancelled before receiving a response"
    );
    assert_eq!(port.asked().len(), 1);
}

/// A run whose host can ask nobody is told the tool is not enabled.
#[tokio::test]
async fn a_run_without_a_port_is_told_the_tool_is_not_enabled() {
    let output = observe(
        &default_mode_tool(),
        &run(),
        &ToolServices::new(),
        &confirm_arguments(),
    )
    .await;

    assert_eq!(
        body(&output),
        "request_user_input is not enabled for this run"
    );
}
