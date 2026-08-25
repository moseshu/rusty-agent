//! Provider-wire tool-schema snapshot and byte-stability contract.

use std::path::PathBuf;

use ra_coding::CodingHost;
use ra_core::{
    item::{AgentId, Message, ModelInputItem},
    model::{
        Model, ModelHandoffDefinition, ModelRequest, ModelSettings, ModelToolDefinition,
        ProviderKey,
    },
    tool::Tool,
};
use ra_model::{
    anthropic::smoke::preview_request_payload,
    openai::{auth::OpenAiAuth, chat::OpenAiChatModel, responses::OpenAiResponsesModel},
};
use ra_tools::{exec_command::ExecCommandTool, read_file::ReadFileTool};
use serde_json::{Value, json};
use tempfile::TempDir;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

const RENDER_COUNT: usize = 100;
const SNAPSHOT: &str = "tool-schemas.txt";

fn snapshot_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../api")
        .join(SNAPSHOT)
}

fn resolved_settings() -> ra_core::model::ResolvedModelSettings {
    let settings = ModelSettings::new().with_max_tokens(256);
    ModelSettings::new().resolve(
        &ProviderKey::new("openai"),
        &ModelSettings::new(),
        &ModelSettings::new(),
        &settings,
    )
}

fn tool_definitions(workspace: &TempDir) -> Vec<ModelToolDefinition> {
    let host = CodingHost::open(workspace.path()).expect("coding host builds");
    let read_file = ReadFileTool::new().expect("read_file builds");
    let exec_command = ExecCommandTool::new().expect("exec_command builds");
    let apply_patch = host.apply_patch_tool().expect("apply_patch builds");
    vec![
        read_file.model_definition(),
        exec_command.model_definition(),
        apply_patch.model_definition(),
    ]
}

fn handoff_definition() -> ModelHandoffDefinition {
    ModelHandoffDefinition::new(
        AgentId::new("researcher"),
        "delegate_research",
        json!({
            "type": "object",
            "properties": {
                "topic": {"type": "string", "description": "Research topic"},
                "depth": {"minimum": 1, "type": "integer"}
            },
            "additionalProperties": false
        }),
    )
    .with_description("Delegate research to a specialist.")
    .with_strict(true)
}

fn model_request(workspace: &TempDir) -> ModelRequest {
    ModelRequest::new(
        vec![ModelInputItem::Message(Message::user("inspect schemas"))],
        resolved_settings(),
    )
    .with_tools(tool_definitions(workspace))
    .with_handoffs(vec![handoff_definition()])
}

async fn responses_tools(request: ModelRequest) -> Value {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "resp_schema",
            "output": []
        })))
        .mount(&server)
        .await;
    let model = OpenAiResponsesModel::new(
        "schema-test",
        OpenAiAuth::new("test-secret").with_base_url(format!("{}/v1/", server.uri())),
    )
    .expect("responses model builds");
    model
        .get_response(request)
        .await
        .expect("responses request lowers");
    let requests = server
        .received_requests()
        .await
        .expect("wiremock retains requests");
    serde_json::from_slice::<Value>(&requests[0].body).expect("responses body is JSON")["tools"]
        .clone()
}

async fn chat_tools(request: ModelRequest) -> Value {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "chat_schema",
            "choices": [{
                "index": 0,
                "finish_reason": "stop",
                "message": {"role": "assistant", "content": "ok"}
            }]
        })))
        .mount(&server)
        .await;
    let model = OpenAiChatModel::new(
        "schema-test",
        OpenAiAuth::new("test-secret").with_base_url(format!("{}/v1/", server.uri())),
    )
    .expect("chat model builds");
    model
        .get_response(request)
        .await
        .expect("chat request lowers");
    let requests = server
        .received_requests()
        .await
        .expect("wiremock retains requests");
    serde_json::from_slice::<Value>(&requests[0].body).expect("chat body is JSON")["tools"].clone()
}

async fn render_snapshot() -> String {
    let workspace = tempfile::tempdir().expect("workspace");
    let responses = responses_tools(model_request(&workspace)).await;
    let chat = chat_tools(model_request(&workspace)).await;
    let anthropic = preview_request_payload("schema-test", &model_request(&workspace))
        .expect("Anthropic preview lowers")["tools"]
        .clone();

    [
        ("openai_responses", responses),
        ("openai_chat", chat),
        ("anthropic_messages", anthropic),
    ]
    .into_iter()
    .map(|(protocol, tools)| {
        let json = serde_json::to_string(&tools).expect("tool payload is serializable");
        format!("### {protocol}\n{json}\n")
    })
    .collect()
}

#[tokio::test]
async fn test_tool_schema_dump_01() {
    let rendered = render_snapshot().await;
    let path = snapshot_path();
    if std::env::var_os("BLESS_TOOL_SCHEMA_DUMP").is_some() {
        std::fs::write(&path, rendered).expect("snapshot must be writable");
        return;
    }

    let baseline = std::fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!(
            "missing tool-schema snapshot at {}: {error}. Run with BLESS_TOOL_SCHEMA_DUMP=1 to create it",
            path.display()
        )
    });
    assert_eq!(
        baseline, rendered,
        "provider tool schema changed; the stable request prefix changes with it. \
         Re-run with BLESS_TOOL_SCHEMA_DUMP=1 and let the diff be reviewed"
    );
}

#[tokio::test]
async fn test_tool_schema_dump_02() {
    let expected = render_snapshot().await;
    for iteration in 2..=RENDER_COUNT {
        assert_eq!(
            render_snapshot().await,
            expected,
            "provider tool payload changed at render {iteration}"
        );
    }
}
