//! `view_image`: the explicit "look at this" entry, and what it refuses to look at.

use ra_core::{
    agent::AgentSpec,
    context::RunContext,
    item::{AgentId, CallId, ImageSource},
    permission::ActionClass,
    state::RunId,
    tool::{Tool, ToolConcurrency, ToolContext, ToolOutput, ToolOutputBlock},
};
use ra_exec::fs::Workspace;
use ra_tools::view_image::{ViewImageLimits, ViewImageTool};
use serde_json::{Value, json};
use tempfile::TempDir;

/// A complete, decodable one-pixel PNG.
const IMAGE_BYTES: &[u8] = include_bytes!("fixtures/view_image/pixel.png");

fn workspace_with(name: &str, contents: impl AsRef<[u8]>) -> TempDir {
    let directory = tempfile::tempdir().expect("a temporary workspace");
    std::fs::write(directory.path().join(name), contents).expect("the fixture file");
    directory
}

fn tool(directory: &TempDir) -> ViewImageTool {
    let workspace = Workspace::open(directory.path()).expect("workspace opens");
    ViewImageTool::for_workspace(&workspace).expect("view_image builds")
}

fn run() -> RunContext {
    let agent = AgentSpec::builder()
        .id(AgentId::new("viewer"))
        .name("Viewer")
        .build()
        .expect("an agent");
    RunContext::new(RunId::new("run-view-image"), agent.as_ref())
}

/// Runs the call and, when it fails, the tool's own failure shaping.
async fn observe(tool: &ViewImageTool, arguments: &Value) -> ToolOutput {
    let call_id = CallId::new("call-view");
    let run = run();
    let context = || ToolContext::new(&run, tool, &call_id, arguments);
    match tool.call(context()).await {
        Ok(output) => output,
        Err(error) => tool
            .handle_failure(&context(), &error)
            .await
            .expect("failure shaping must not fail")
            .expect("view_image shapes every failure it produces"),
    }
}

fn refusal(output: &ToolOutput) -> &str {
    output.as_text().expect("a refusal is one text block")
}

/// The attached bytes and the media type a provider is told, or a panic naming what came back.
fn attached(output: &ToolOutput) -> (&str, &str) {
    match output.blocks() {
        [ToolOutputBlock::Image(image)] => match image.source() {
            ImageSource::Base64(source) => (source.media_type(), source.data()),
            other => panic!("the image did not travel inline: {other:?}"),
        },
        other => panic!("expected one image block, got {other:?}"),
    }
}

/// The entry advertises what it is, and claims the workspace it shares with the other readers.
#[tokio::test]
async fn test_the_image_entry_advertises_a_strict_schema_and_shares_the_workspace() {
    let directory = workspace_with("diagram.png", IMAGE_BYTES);
    let tool = tool(&directory);

    tool.validate().expect("identity and schema must agree");
    assert_eq!(tool.origin().qualified_name(), "view_image");
    assert!(tool.schema().strict_json_schema());
    assert_eq!(tool.schema().input_schema()["required"], json!(["path"]));
    assert_eq!(
        tool.options().action_classes(),
        &[ActionClass::from("read")]
    );
    assert_eq!(tool.options().concurrency(), ToolConcurrency::Parallel);
    // A shared claim rather than none: reads run alongside each other, and alongside anything else
    // that only observes the same workspace.
    assert_eq!(tool.options().resource_claims().len(), 1);
}

/// A workspace image comes back as one inline image block with its decoded media type.
#[tokio::test]
async fn test_an_image_is_attached_with_its_decoded_media_type() {
    let directory = workspace_with("diagram.png", IMAGE_BYTES);
    let output = observe(&tool(&directory), &json!({ "path": "diagram.png" })).await;

    let (media_type, data) = attached(&output);
    assert_eq!(media_type, "image/png");
    assert!(!data.is_empty(), "the image arrived with no bytes");
    // The path is named in guidance rather than in a text block of its own, so the result stays one
    // image for a provider and still says which file it is.
    assert!(
        output
            .metadata()
            .guidance()
            .join(" ")
            .contains("diagram.png"),
        "{:?}",
        output.metadata().guidance()
    );
}

/// An absolute path inside the workspace is the same request written another way.
///
/// It is built from the tool's own root rather than from the temporary directory, because those are
/// not always the same string: opening a workspace canonicalizes, so on a host where `/var` is a
/// link to `/private/var` the two spellings differ. The comparison is lexical on purpose — matching
/// aliases would mean canonicalizing model input before the open, which is the first half of the
/// race the descriptor exists to avoid — so this test names the path the tool would recognize.
#[tokio::test]
async fn test_an_absolute_path_inside_the_workspace_is_accepted() {
    let directory = workspace_with("diagram.jpg", IMAGE_BYTES);
    let tool = tool(&directory);
    let absolute = tool.root().join("diagram.jpg");
    let output = observe(&tool, &json!({ "path": absolute.to_string_lossy() })).await;

    assert_eq!(attached(&output).0, "image/png");
}

/// Codex rejects invalid image data before returning an image block.
#[tokio::test]
async fn invalid_or_unsupported_data_never_reaches_an_image_block() {
    for (path, bytes) in [
        ("chart.svg", b"<svg></svg>".as_slice()),
        ("broken.png", b"\x89PNG".as_slice()),
        ("empty.png", b"".as_slice()),
        ("invalid-image.json", br#"{"message":"hello"}"#.as_slice()),
    ] {
        let directory = workspace_with(path, bytes);
        let output = observe(&tool(&directory), &json!({"path":path})).await;
        assert_eq!(
            refusal(&output),
            "unable to process image: invalid or unsupported image data"
        );
        assert!(output.metadata().guidance().is_empty());
    }
}

/// A valid BMP is unsupported, even when its filename claims a provider-supported format.
#[tokio::test]
async fn valid_bmp_is_rejected_before_an_image_block_is_created() {
    // A one-pixel, uncompressed 24-bit BMP with a complete file and bitmap header.
    let bytes = include_bytes!("fixtures/view_image/pixel.bmp");
    for path in ["image.bmp", "disguised.png"] {
        let directory = workspace_with(path, bytes);
        let output = observe(&tool(&directory), &json!({"path":path})).await;
        assert_eq!(
            refusal(&output),
            "unable to process image: invalid or unsupported image data"
        );
        assert!(output.metadata().guidance().is_empty());
    }
}

/// Valid image bytes do not depend on a matching filename extension.
#[tokio::test]
async fn image_data_is_decoded_even_without_an_image_extension() {
    use base64::Engine as _;
    let directory = workspace_with("image.data", IMAGE_BYTES);
    let output = observe(&tool(&directory), &json!({"path":"image.data"})).await;
    let (media_type, data) = attached(&output);
    assert_eq!(media_type, "image/png");
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(data)
            .unwrap(),
        IMAGE_BYTES
    );
}

/// Leaving the workspace and spelling a path with `..` are different refusals.
///
/// Only the first is a boundary violation. `src/../logo.png` is very often inside the root, and
/// telling the model it is outside sends it looking for a problem it does not have.
#[tokio::test]
async fn test_the_two_path_refusals_say_different_things() {
    let directory = workspace_with("logo.png", IMAGE_BYTES);
    let tool = tool(&directory);

    let outside = observe(&tool, &json!({ "path": "/etc/hosts.png" })).await;
    assert!(
        refusal(&outside).contains("outside the workspace"),
        "{}",
        refusal(&outside)
    );

    let parent = observe(&tool, &json!({ "path": "sub/../logo.png" })).await;
    assert!(refusal(&parent).contains("`..`"), "{}", refusal(&parent));
}

/// An empty file and an oversized one are refused rather than sent as blocks.
///
/// The empty case matters on its own: a zero-byte image is a block a provider rejects outright, so
/// a run that sent one would fail on the request rather than on the call that produced it.
#[tokio::test]
async fn test_an_empty_or_oversized_image_is_refused() {
    let directory = workspace_with("blank.png", "");
    let empty = observe(&tool(&directory), &json!({ "path": "blank.png" })).await;
    assert_eq!(
        refusal(&empty),
        "unable to process image: invalid or unsupported image data"
    );

    let directory = workspace_with("large.png", vec![0_u8; 64]);
    let workspace = Workspace::open(directory.path()).expect("workspace opens");
    let narrow = ViewImageTool::for_workspace(&workspace)
        .expect("view_image builds")
        .with_limits(ViewImageLimits::new().with_max_bytes(16));
    let large = observe(&narrow, &json!({ "path": "large.png" })).await;
    assert!(
        refusal(&large).contains("over the 16 byte limit"),
        "{}",
        refusal(&large)
    );
}

/// A link pointing out of the workspace is refused, whatever it is named.
///
/// The confinement is the descriptor's, not the extension check's: the path passes every lexical
/// rule and still cannot be opened, which is what makes the check-then-open race unreachable.
#[cfg(unix)]
#[tokio::test]
async fn test_a_link_out_of_the_workspace_is_refused() {
    let outside = workspace_with("secret.png", IMAGE_BYTES);
    let directory = tempfile::tempdir().expect("a temporary workspace");
    std::os::unix::fs::symlink(
        outside.path().join("secret.png"),
        directory.path().join("link.png"),
    )
    .expect("the link");

    let output = observe(&tool(&directory), &json!({ "path": "link.png" })).await;

    assert!(
        refusal(&output).contains("outside the workspace"),
        "{}",
        refusal(&output)
    );
}
