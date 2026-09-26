//! `ra-tools::sandbox::view_image`: the sandbox `view_image` tool against a scripted session.
//!
//! Ported from the reference's `tests/sandbox/capabilities/test_view_image_tool.py`, then
//! `tests/sandbox/test_view_image_content_validation.py`, then the scripted half of
//! `tests/sandbox/test_posix_tool_paths.py`, each in upstream order. The session records every read
//! and answers from a script, as the reference's `scripted_sandbox_session` does.
//!
//! The reference's image output is a data URL; here it is an image block with the media type and
//! the base64 payload the adapter renders into that URL, and the tests read those two parts.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use ra_core::{
    agent::AgentSpec,
    context::RunContext,
    error::{Error, ToolErrorKind},
    item::{AgentId, CallId, ImageSource},
    sandbox::{
        AsUser, ErrorCode, ExecRequest, ExecResult, FileEntry, Manifest, OpName, SandboxError,
        SandboxPathGrant, SandboxResult, SandboxSession, SandboxSessionState,
        SandboxWorkspaceScope, SessionResources, Snapshot, User,
    },
    state::RunId,
    tool::{Tool, ToolApprovalPolicy, ToolConcurrency, ToolContext, ToolOutput, ToolOutputBlock},
};
use ra_tools::sandbox::NeedsApproval;
use ra_tools::sandbox::shell_tool::resolve_workdir_command;
use ra_tools::sandbox::view_image::{
    MAX_IMAGE_BYTES, ViewImageArgs, ViewImageTool, detect_image_mime_type,
};
use serde_json::{Value, json};

const PNG_BASE64: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+a84QAAAAASUVORK5CYII=";

fn png() -> Vec<u8> {
    BASE64.decode(PNG_BASE64).expect("png")
}

fn huge_png() -> Vec<u8> {
    let mut payload = b"\x89PNG\r\n\x1a\n".to_vec();
    payload.resize(payload.len() + MAX_IMAGE_BYTES + 1, b'0');
    payload
}

// ---- the scripted session ------------------------------------------------------------------

struct ScriptedSession {
    state: SandboxSessionState,
    resources: SessionResources,
    script: Mutex<VecDeque<SandboxResult<Vec<u8>>>>,
    reads: Mutex<Vec<(String, AsUser)>>,
    /// The byte limit of each read, `None` for an unbounded one.
    limits: Mutex<Vec<Option<u64>>>,
}

impl ScriptedSession {
    fn new(script: Vec<SandboxResult<Vec<u8>>>) -> Arc<Self> {
        Self::with_manifest(script, Manifest::new().with_root("/workspace"))
    }

    fn with_manifest(script: Vec<SandboxResult<Vec<u8>>>, manifest: Manifest) -> Arc<Self> {
        Arc::new(Self {
            state: SandboxSessionState::new("scripted", Snapshot::noop(), manifest),
            resources: SessionResources::new(),
            script: Mutex::new(script.into()),
            reads: Mutex::new(Vec::new()),
            limits: Mutex::new(Vec::new()),
        })
    }

    fn reads(&self) -> Vec<(String, AsUser)> {
        self.reads.lock().unwrap().clone()
    }

    fn limits(&self) -> Vec<Option<u64>> {
        self.limits.lock().unwrap().clone()
    }

    fn read_paths(&self) -> Vec<String> {
        self.reads().into_iter().map(|(path, _)| path).collect()
    }

    fn assert_complete(&self) {
        assert!(
            self.script.lock().unwrap().is_empty(),
            "unused script steps"
        );
    }
}

fn not_scripted() -> SandboxError {
    SandboxError::new(
        ErrorCode::SandboxConfigInvalid,
        OpName::Exec,
        "not scripted",
    )
}

#[async_trait]
impl SandboxSession for ScriptedSession {
    fn backend_id(&self) -> &str {
        "scripted"
    }

    fn state(&self) -> SandboxSessionState {
        self.state.clone()
    }

    fn resources(&self) -> &SessionResources {
        &self.resources
    }

    async fn exec(&self, _request: ExecRequest) -> SandboxResult<ExecResult> {
        Err(not_scripted())
    }

    async fn running(&self) -> SandboxResult<bool> {
        Ok(true)
    }

    async fn ls(&self, _path: &str, _user: AsUser) -> SandboxResult<Vec<FileEntry>> {
        Err(not_scripted())
    }

    async fn rm(&self, _path: &str, _recursive: bool, _user: AsUser) -> SandboxResult<()> {
        Err(not_scripted())
    }

    async fn mkdir(&self, _path: &str, _parents: bool, _user: AsUser) -> SandboxResult<()> {
        Err(not_scripted())
    }

    async fn read(&self, path: &str, user: AsUser) -> SandboxResult<Vec<u8>> {
        self.limits.lock().unwrap().push(None);
        self.answer(path, user)
    }

    async fn read_up_to(&self, path: &str, user: AsUser, max_bytes: u64) -> SandboxResult<Vec<u8>> {
        self.limits.lock().unwrap().push(Some(max_bytes));
        let mut data = self.answer(path, user)?;
        data.truncate(usize::try_from(max_bytes).unwrap());
        Ok(data)
    }

    async fn write(&self, _path: &str, _data: Vec<u8>, _user: AsUser) -> SandboxResult<()> {
        Err(not_scripted())
    }

    async fn persist_workspace(&self) -> SandboxResult<Vec<u8>> {
        Err(not_scripted())
    }

    async fn hydrate_workspace(&self, _data: Vec<u8>) -> SandboxResult<()> {
        Err(not_scripted())
    }
}

impl ScriptedSession {
    fn answer(&self, path: &str, user: AsUser) -> SandboxResult<Vec<u8>> {
        self.reads.lock().unwrap().push((path.to_owned(), user));
        self.script
            .lock()
            .unwrap()
            .pop_front()
            .expect("the script has an answer for this read")
    }
}

fn session(scripted: &Arc<ScriptedSession>) -> Arc<dyn SandboxSession> {
    Arc::clone(scripted) as Arc<dyn SandboxSession>
}

fn tool(scripted: &Arc<ScriptedSession>) -> ViewImageTool {
    ViewImageTool::new(session(scripted)).expect("tool")
}

fn scope(cwd: &str) -> SandboxWorkspaceScope {
    SandboxWorkspaceScope::from_cwd(Some(cwd)).expect("scope")
}

fn run_context() -> RunContext {
    let agent = AgentSpec::builder()
        .id(AgentId::new("image-viewer"))
        .name("Image viewer")
        .build()
        .expect("agent");
    RunContext::new(RunId::new("run-sandbox-view-image"), &agent)
}

/// Calls the tool through its `Tool` entry, as the runtime would.
async fn invoke(tool: &ViewImageTool, arguments: &Value) -> Result<ToolOutput, Error> {
    let run = run_context();
    let call_id = CallId::new("call");
    tool.call(ToolContext::new(&run, tool, &call_id, arguments))
        .await
}

/// The image's media type and base64 payload.
fn image(output: &ToolOutput) -> (String, String) {
    match output.blocks() {
        [ToolOutputBlock::Image(block)] => {
            assert_eq!(block.detail(), None);
            match block.source() {
                ImageSource::Base64(source) => {
                    (source.media_type().to_owned(), source.data().to_owned())
                }
                other => panic!("expected an inline image, got {other:?}"),
            }
        }
        other => panic!("expected one image block, got {other:?}"),
    }
}

fn text(output: &ToolOutput) -> &str {
    output.as_text().expect("a text response")
}

// ---- test_view_image_tool.py ---------------------------------------------------------------

#[tokio::test]
async fn view_image_takes_a_per_call_approval_check() {
    let scripted = ScriptedSession::new(Vec::new());
    let check = |context: &ToolContext<'_>| {
        Ok(context.arguments()["path"]
            .as_str()
            .is_some_and(|path| path.starts_with("sensitive/")))
    };
    let tool = tool(&scripted).with_needs_approval(NeedsApproval::check(check));

    assert!(matches!(
        tool.needs_approval_policy(),
        NeedsApproval::Check(_)
    ));
    assert_eq!(tool.options().approval(), ToolApprovalPolicy::Dynamic);

    let run = run_context();
    let call_id = CallId::new("call");
    let sensitive = json!({"path": "sensitive/a.png"});
    let plain = json!({"path": "images/a.png"});
    assert!(
        tool.needs_approval(&ToolContext::new(&run, &tool, &call_id, &sensitive))
            .await
            .unwrap()
    );
    assert!(
        !tool
            .needs_approval(&ToolContext::new(&run, &tool, &call_id, &plain))
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn a_png_comes_back_as_an_image() {
    let scripted = ScriptedSession::new(vec![Ok(png())]);

    let output = invoke(&tool(&scripted), &json!({"path": "images/dot.png"}))
        .await
        .unwrap();

    assert_eq!(
        image(&output),
        ("image/png".to_owned(), PNG_BASE64.to_owned())
    );
    assert_eq!(scripted.read_paths(), vec!["/workspace/images/dot.png"]);
    scripted.assert_complete();
}

#[tokio::test]
async fn an_absolute_path_under_a_grant_is_read() {
    let grant = SandboxPathGrant::new("/shared")
        .expect("grant")
        .read_only(true);
    let scripted = ScriptedSession::with_manifest(
        vec![Ok(png())],
        Manifest::new()
            .with_root("/workspace")
            .with_path_grant(grant),
    );

    let output = invoke(&tool(&scripted), &json!({"path": "/shared/dot.png"}))
        .await
        .unwrap();

    image(&output);
    assert_eq!(scripted.read_paths(), vec!["/shared/dot.png"]);
    scripted.assert_complete();
}

#[tokio::test]
async fn an_absolute_path_without_a_grant_is_refused() {
    let scripted = ScriptedSession::new(Vec::new());

    let error = invoke(&tool(&scripted), &json!({"path": "/shared/dot.png"}))
        .await
        .unwrap_err();

    let source = std::error::Error::source(&error)
        .and_then(|source| source.downcast_ref::<SandboxError>())
        .expect("the session's refusal is the source");
    assert_eq!(source.error_code(), ErrorCode::InvalidManifestPath);
    assert!(scripted.reads().is_empty());
}

#[tokio::test]
async fn a_relative_path_is_measured_from_the_working_directory() {
    let scripted = ScriptedSession::new(vec![Ok(png())]);
    let tool = tool(&scripted).with_workspace_scope(scope("tasks/a"));

    let output = invoke(&tool, &json!({"path": "images/dot.png"}))
        .await
        .unwrap();

    image(&output);
    assert_eq!(
        scripted.read_paths(),
        vec!["/workspace/tasks/a/images/dot.png"]
    );
    scripted.assert_complete();
}

#[tokio::test]
async fn backslashes_are_separators_before_the_working_directory_is_applied() {
    let scripted = ScriptedSession::new(vec![Ok(png())]);
    let tool = tool(&scripted).with_workspace_scope(scope("tasks/a"));

    let output = tool
        .run(&ViewImageArgs::new(r"images\dot.png"))
        .await
        .unwrap();

    image(&output);
    assert_eq!(
        scripted.read_paths(),
        vec!["/workspace/tasks/a/images/dot.png"]
    );
    scripted.assert_complete();
}

#[tokio::test]
async fn an_absolute_path_is_taken_at_face_value_under_a_working_directory() {
    let scripted = ScriptedSession::new(vec![Ok(b"hello\n".to_vec())]);
    let tool = tool(&scripted).with_workspace_scope(scope("tasks/a"));

    let output = invoke(&tool, &json!({"path": "/workspace/notes.txt"}))
        .await
        .unwrap();

    assert_eq!(
        text(&output),
        "image path `notes.txt` is not a supported image file"
    );
    assert_eq!(scripted.read_paths(), vec!["/workspace/notes.txt"]);
    scripted.assert_complete();
}

#[tokio::test]
async fn a_backslashed_absolute_path_is_reported_after_normalization() {
    let scripted = ScriptedSession::new(vec![Ok(b"hello\n".to_vec())]);
    let tool = tool(&scripted).with_workspace_scope(scope("tasks/a"));

    let output = tool
        .run(&ViewImageArgs::new(r"\workspace\root.txt"))
        .await
        .unwrap();

    assert_eq!(
        text(&output),
        "image path `root.txt` is not a supported image file"
    );
    assert_eq!(scripted.read_paths(), vec!["/workspace/root.txt"]);
    scripted.assert_complete();
}

#[tokio::test]
async fn a_missing_image_under_a_working_directory_is_named_as_the_model_named_it() {
    let scripted = ScriptedSession::new(vec![Err(SandboxError::workspace_read_not_found(
        "/provider/private/root/tasks/a/images/missing.png",
    ))]);
    let tool = tool(&scripted).with_workspace_scope(scope("tasks/a"));

    let output = invoke(&tool, &json!({"path": "images/missing.png"}))
        .await
        .unwrap();

    assert_eq!(
        text(&output),
        "image path `images/missing.png` was not found"
    );
    assert!(!text(&output).contains("/provider/private/root"));
    assert_eq!(
        scripted.read_paths(),
        vec!["/workspace/tasks/a/images/missing.png"]
    );
    scripted.assert_complete();
}

#[tokio::test]
async fn an_image_is_read_as_the_bound_user() {
    let scripted = ScriptedSession::new(vec![Ok(png())]);
    let tool = tool(&scripted).with_user(Some(User::new("sandbox-user")));

    let output = invoke(&tool, &json!({"path": "images/dot.png"}))
        .await
        .unwrap();

    image(&output);
    assert_eq!(scripted.reads()[0].1, Some(User::new("sandbox-user")));
    scripted.assert_complete();
}

#[tokio::test]
async fn a_file_that_is_not_an_image_is_refused() {
    let scripted = ScriptedSession::new(vec![Ok(b"hello\n".to_vec())]);

    let output = invoke(&tool(&scripted), &json!({"path": "notes.txt"}))
        .await
        .unwrap();

    assert_eq!(
        text(&output),
        "image path `notes.txt` is not a supported image file"
    );
    scripted.assert_complete();
}

#[tokio::test]
async fn an_image_over_10mb_is_refused() {
    let scripted = ScriptedSession::new(vec![Ok(huge_png())]);

    let output = invoke(&tool(&scripted), &json!({"path": "images/huge.png"}))
        .await
        .unwrap();

    // Read one byte past the ceiling, and no more: the reference's `read(_MAX_IMAGE_BYTES + 1)`.
    assert_eq!(
        scripted.limits(),
        vec![Some(u64::try_from(MAX_IMAGE_BYTES).unwrap() + 1)]
    );

    assert_eq!(
        text(&output),
        "image path `images/huge.png` exceeded the allowed size of 10MB; resize or compress the \
         image and try again"
    );
    scripted.assert_complete();
}

#[tokio::test]
async fn no_refusal_shows_where_the_backend_keeps_the_workspace() {
    let provider_root = "/provider/private/root";
    let scripted = ScriptedSession::with_manifest(
        vec![
            Err(SandboxError::workspace_read_not_found(&format!(
                "{provider_root}/images/missing.png"
            ))),
            Ok(b"hello\n".to_vec()),
            Ok(huge_png()),
        ],
        Manifest::new().with_root(provider_root),
    );
    let tool = tool(&scripted);

    let mut outputs = Vec::new();
    for path in ["images/missing.png", "notes.txt", "images/huge.png"] {
        let output = invoke(&tool, &json!({"path": path})).await.unwrap();
        outputs.push(text(&output).to_owned());
    }

    assert_eq!(
        outputs,
        vec![
            "image path `images/missing.png` was not found",
            "image path `notes.txt` is not a supported image file",
            "image path `images/huge.png` exceeded the allowed size of 10MB; resize or compress \
             the image and try again",
        ]
    );
    for output in &outputs {
        assert!(!output.contains(provider_root), "{output}");
    }
}

// ---- test_view_image_content_validation.py -------------------------------------------------

#[tokio::test]
async fn bytes_that_are_not_an_image_are_refused_whatever_the_extension() {
    let scripted = ScriptedSession::new(vec![Ok(b"not an image\n".to_vec())]);

    let output = tool(&scripted)
        .run(&ViewImageArgs::new("images/fake.png"))
        .await
        .unwrap();

    assert_eq!(
        text(&output),
        "image path `images/fake.png` is not a supported image file"
    );
    scripted.assert_complete();
}

// `test_view_image_ignores_mutated_mime_mapping_for_raster_extension` replaces Python's
// `mimetypes.guess_type` to prove it is never consulted. Nothing here has a MIME table to replace:
// detection is `detect_image_mime_type` over the bytes, which the test above already exercises
// with a raster extension.

#[tokio::test]
async fn an_image_signature_is_enough_without_an_image_extension() {
    let scripted = ScriptedSession::new(vec![Ok(png())]);

    let output = tool(&scripted)
        .run(&ViewImageArgs::new("images/payload.bin"))
        .await
        .unwrap();

    assert_eq!(image(&output).0, "image/png");
    scripted.assert_complete();
}

#[tokio::test]
async fn an_svg_named_svg_is_an_image_however_it_is_encoded() {
    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg"></svg>"#;
    let utf16: Vec<u8> = [0xff_u8, 0xfe]
        .into_iter()
        .chain(svg.encode_utf16().flat_map(u16::to_le_bytes))
        .collect();
    let payloads: Vec<(&str, Vec<u8>)> = vec![
        (
            "utf8-bom",
            [b"\xef\xbb\xbf".as_slice(), svg.as_bytes()].concat(),
        ),
        (
            "comment",
            [b"<!-- generated -->\n".as_slice(), svg.as_bytes()].concat(),
        ),
        (
            "doctype",
            [
                br#"<!DOCTYPE svg PUBLIC "-//W3C//DTD SVG 1.1//EN" "svg11.dtd">"#.as_slice(),
                b"\n",
                svg.as_bytes(),
            ]
            .concat(),
        ),
        ("utf16", utf16),
    ];

    for (name, payload) in payloads {
        let scripted = ScriptedSession::new(vec![Ok(payload)]);
        let output = tool(&scripted)
            .run(&ViewImageArgs::new("images/vector.svg"))
            .await
            .unwrap();
        assert_eq!(image(&output).0, "image/svg+xml", "{name}");
        scripted.assert_complete();
    }
}

#[tokio::test]
async fn a_file_named_svgz_is_an_svg() {
    // Compressed bytes, which no signature recognises; the name is what makes it SVG.
    let svgz = b"\x1f\x8b\x08\x00\x00\x00\x00\x00\x02\xff\xb3)".to_vec();
    let scripted = ScriptedSession::new(vec![Ok(svgz)]);

    let output = tool(&scripted)
        .run(&ViewImageArgs::new("images/vector.svgz"))
        .await
        .unwrap();

    assert_eq!(image(&output).0, "image/svg+xml");
    scripted.assert_complete();
}

// ---- test_posix_tool_paths.py, the scripted half ------------------------------------------

#[tokio::test]
async fn a_shell_workdir_reads_backslashes_as_separators() {
    let scripted = ScriptedSession::new(Vec::new());

    let command = resolve_workdir_command(
        scripted.as_ref(),
        &SandboxWorkspaceScope::root(),
        "pwd",
        Some(r"src\project"),
    )
    .await
    .unwrap();

    assert_eq!(command, "cd /workspace/src/project && pwd");
}

#[tokio::test]
async fn view_image_reads_backslashes_as_separators() {
    let scripted = ScriptedSession::new(vec![Ok(png())]);

    let output = tool(&scripted)
        .run(&ViewImageArgs::new(r"images\plot.png"))
        .await
        .unwrap();

    image(&output);
    assert_eq!(scripted.read_paths(), vec!["/workspace/images/plot.png"]);
    scripted.assert_complete();
}

// ---- beyond the reference's tests ----------------------------------------------------------

#[test]
fn the_tool_is_parallel_needs_no_approval_and_has_the_references_schema() {
    let scripted = ScriptedSession::new(Vec::new());
    let tool = tool(&scripted);

    assert_eq!(tool.options().approval(), ToolApprovalPolicy::Never);
    assert_eq!(tool.options().concurrency(), ToolConcurrency::Parallel);

    let schema = tool.schema();
    assert_eq!(schema.name(), "view_image");
    assert_eq!(
        schema.description(),
        Some(
            "Loads an image from the sandbox workspace or an explicitly granted sandbox path and \
             returns it as a structured image output."
        )
    );
    assert!(!schema.strict_json_schema());
    let input = schema.input_schema();
    assert_eq!(input["required"], json!(["path"]));
    assert_eq!(input["properties"]["path"]["type"], json!("string"));
    assert_eq!(input["properties"]["path"]["minLength"], json!(1));
    assert_eq!(
        input["properties"]["path"]["description"],
        json!(
            "Path to the image file. Workspace paths and explicitly granted sandbox paths are \
             supported."
        )
    );
}

#[tokio::test]
async fn an_empty_path_is_invalid_input_and_reads_nothing() {
    let scripted = ScriptedSession::new(Vec::new());

    let error = invoke(&tool(&scripted), &json!({"path": ""}))
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        Error::Tool {
            kind: ToolErrorKind::InvalidInput,
            ..
        }
    ));
    assert!(scripted.reads().is_empty());
}

#[tokio::test]
async fn a_failed_read_names_the_failure() {
    let scripted = ScriptedSession::new(vec![Err(SandboxError::workspace_archive_read(
        "/workspace/images/dot.png",
    ))]);

    let output = invoke(&tool(&scripted), &json!({"path": "images/dot.png"}))
        .await
        .unwrap();

    assert_eq!(
        text(&output),
        "unable to read image at `images/dot.png`: workspace_archive_read_error"
    );
}

#[test]
fn every_signature_the_reference_recognises_is_recognised() {
    let cases: [(&[u8], Option<&str>); 9] = [
        (b"\x89PNG\r\n\x1a\nrest", Some("image/png")),
        (b"\xff\xd8\xffrest", Some("image/jpeg")),
        (b"GIF87a", Some("image/gif")),
        (b"GIF89a", Some("image/gif")),
        (b"RIFF\0\0\0\0WEBPVP8 ", Some("image/webp")),
        (b"BMrest", Some("image/bmp")),
        (b"II*\x00", Some("image/tiff")),
        (b"MM\x00*", Some("image/tiff")),
        (
            b"\x0b\x0c <?XML version='1.0'?><SVG>",
            Some("image/svg+xml"),
        ),
    ];
    for (payload, expected) in cases {
        assert_eq!(
            detect_image_mime_type("file.bin", payload),
            expected,
            "{payload:?}"
        );
    }
    assert_eq!(
        detect_image_mime_type("file.bin", b"RIFF\0\0\0\0WAVE"),
        None
    );
    assert_eq!(detect_image_mime_type("dir/.svg", b"x"), None);
    assert_eq!(
        detect_image_mime_type("dir/a.SVG", b"x"),
        Some("image/svg+xml")
    );
    assert_eq!(detect_image_mime_type("dir/a.svg.", b"x"), None);
}

/// Every image read is bounded, so no file is ever held whole: the ceiling is enforced by how much
/// is read, not only by what is said afterwards.
#[tokio::test]
async fn every_image_read_is_bounded_to_one_byte_past_the_ceiling() {
    let scripted = ScriptedSession::new(vec![
        Ok(png()),
        Ok(b"hello\n".to_vec()),
        Err(SandboxError::workspace_read_not_found(
            "/workspace/gone.png",
        )),
    ]);
    let tool = tool(&scripted);

    for path in ["dot.png", "notes.txt", "gone.png"] {
        invoke(&tool, &json!({"path": path})).await.unwrap();
    }

    let bound = Some(u64::try_from(MAX_IMAGE_BYTES).unwrap() + 1);
    assert_eq!(scripted.limits(), vec![bound; 3]);
}
