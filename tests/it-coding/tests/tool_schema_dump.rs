//! Provider-wire tool-schema snapshot and byte-stability contract.

use std::{path::PathBuf, sync::Arc};

#[path = "../../support/configured_coding_host.rs"]
mod configured_host;
#[path = "tool_schema_dump/discovery.rs"]
mod discovery;

use ra_coding::{CodingHost, CodingProfile};
use ra_core::{
    item::{AgentId, Message, ModelInputItem},
    model::{
        Model, ModelHandoffDefinition, ModelRequest, ModelSettings, ModelToolDefinition,
        ProviderKey, SettingsLayers,
    },
};
use ra_model::{
    anthropic::smoke::preview_request_payload,
    openai::{auth::OpenAiAuth, chat::OpenAiChatModel, responses::OpenAiResponsesModel},
};
use serde_json::{Value, json};
use tempfile::TempDir;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

const RENDER_COUNT: usize = 100;
const SNAPSHOT: &str = "tool-schemas.txt";

/// The line the wire measurement comes out on, and that `cargo xtask token-budget` reads back.
///
/// The gate quotes this number rather than only reporting that a test passed, and it goes red when
/// the line stops appearing — which is what keeps the case below from being renamed away unnoticed:
/// `cargo test` run against a name filter that matches nothing exits 0 all the same.
const MEASUREMENT_MARKER: &str = "tool-wire-budget:";

/// The product ceiling, read from the profile that declares it rather than restated here.
///
/// The wire table is the neutral surface plus each provider's envelope, so it is measured against
/// the same ceiling and simply has less room. Reading it back means a change to the declared
/// budget reaches this contract too, instead of leaving a second number to drift.
fn coding_tool_table_ceiling() -> usize {
    CodingProfile::default()
        .to_tool_profile()
        .expect("a valid profile")
        .budget()
        .max_advertised_bytes()
        .expect("the surface declares a byte ceiling")
}

fn snapshot_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../api")
        .join(SNAPSHOT)
}

/// Whether this checkout carries recorded baselines at all.
///
/// `api/` is not committed, so a fresh checkout — CI's among them — has nothing to compare
/// against, and the xtask gates that read the same files report SKIP for it rather than failing.
/// These tests follow the same rule: with the directory absent they still render, say so and
/// stop; with the directory present and a file missing they fail, because that is a baseline
/// nobody blessed.
fn baselines_present() -> bool {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../api")
        .is_dir()
}

fn resolved_settings() -> ra_core::model::ResolvedModelSettings {
    let settings = ModelSettings::new().with_max_tokens(256);
    SettingsLayers::new()
        .with_run(&settings)
        .resolve(&ProviderKey::new("openai"))
}

/// How the host advertises the collaboration tools for one provider path.
#[derive(Clone, Copy)]
enum Collaboration {
    /// Codex's default `collaboration` namespace, which only OpenAI Responses carries.
    Namespaced,
    /// Bare names, which a host serving Chat Completions or Anthropic selects.
    Bare,
}

/// The standard tier assembled from real tools and explicitly installed host services.
async fn tool_definitions(
    workspace: &TempDir,
    collaboration: Collaboration,
) -> Vec<ModelToolDefinition> {
    let services = Arc::new(configured_host::Services::default());
    let mut host =
        services.install(CodingHost::open(workspace.path()).expect("coding host builds"));
    if matches!(collaboration, Collaboration::Bare) {
        host = host.with_collaboration_namespace(None);
    }
    let assembled = ra_coding::host_backed_surface(
        &ra_core::prompt::PromptRole::Main,
        &host,
        CodingProfile::CodexLike,
    )
    .await
    .expect("the configured standard tier assembles");
    let mut definitions: Vec<_> = assembled
        .into_tool_surface()
        .into_tools()
        .iter()
        .map(|tool| tool.model_definition())
        .collect();
    definitions.extend(
        ra_tools::mcp_ns::resource_tools(Vec::new())
            .expect("resource tool schemas need no live connection")
            .into_iter()
            .map(|tool| tool.model_definition()),
    );
    // Native client discovery is Responses-only. Exercise the conditional runtime surface in
    // that provider's wire snapshot; the other providers' refusal is covered in it-model.
    if matches!(collaboration, Collaboration::Namespaced) {
        definitions.push(discovery::definition().await);
    }
    definitions
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

async fn model_request(workspace: &TempDir, collaboration: Collaboration) -> ModelRequest {
    ModelRequest::new(
        vec![ModelInputItem::Message(Message::user("inspect schemas"))],
        resolved_settings(),
    )
    .with_tools(tool_definitions(workspace, collaboration).await)
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
    let responses =
        responses_tools(model_request(&workspace, Collaboration::Namespaced).await).await;
    let chat = chat_tools(model_request(&workspace, Collaboration::Bare).await).await;
    let anthropic = preview_request_payload(
        "schema-test",
        &model_request(&workspace, Collaboration::Bare).await,
    )
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

/// Checks the bytes that each provider actually receives, including its own tool envelope.
///
/// The profile's provider-neutral budget deliberately excludes envelopes because they differ by
/// endpoint. This complementary check keeps that abstraction honest: a provider serializer cannot
/// grow the configured product's wire table past the product ceiling unnoticed. Every static
/// standard tool and all three MCP resource schemas are measured on each provider path. The
/// Responses table also includes the runtime-generated native discovery schema.
#[tokio::test]
async fn test_current_provider_tool_tables_fit_the_coding_byte_budget() {
    let workspace = tempfile::tempdir().expect("workspace");
    let responses =
        responses_tools(model_request(&workspace, Collaboration::Namespaced).await).await;
    let chat = chat_tools(model_request(&workspace, Collaboration::Bare).await).await;
    let anthropic = preview_request_payload(
        "schema-test",
        &model_request(&workspace, Collaboration::Bare).await,
    )
    .expect("Anthropic preview lowers")["tools"]
        .clone();

    let ceiling = coding_tool_table_ceiling();
    let measured = [
        ("openai_responses", responses),
        ("openai_chat", chat),
        ("anthropic_messages", anthropic),
    ]
    .map(|(provider, tools)| {
        let bytes = serde_json::to_vec(&tools)
            .expect("tool payload is serializable")
            .len();
        (provider, bytes)
    });

    // Report the widest one before asserting, so the number is on the record whether or not the
    // assertion holds: the fattest envelope is the one the ceiling will meet first.
    let (widest, bytes) = measured
        .iter()
        .copied()
        .max_by_key(|(_, bytes)| *bytes)
        .expect("three provider paths");
    println!("{MEASUREMENT_MARKER} {widest} {bytes}/{ceiling}");

    for (provider, bytes) in measured {
        assert!(
            bytes <= ceiling,
            "{provider} tool table is {bytes} bytes, above the coding profile ceiling of {ceiling}"
        );
    }
}

#[tokio::test]
async fn test_tool_schema_dump_01() {
    let rendered = render_snapshot().await;
    insta::assert_snapshot!("configured_provider_tools", rendered);
    let path = snapshot_path();
    if std::env::var_os("BLESS_TOOL_SCHEMA_DUMP").is_some() {
        std::fs::write(&path, rendered).expect("snapshot must be writable");
        return;
    }

    if !baselines_present() {
        eprintln!("skipped the comparison: this checkout has no api/ baselines");
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
