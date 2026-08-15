//! R2-8 `read_file`: one multimodal read entry, plus the window and ceiling facts it reports.

use std::io::Write as _;

use ra_core::{
    agent::AgentSpec,
    context::RunContext,
    item::{AgentId, CallId},
    state::RunId,
    tool::{Tool, ToolConcurrency, ToolContext, ToolOutput, ToolOutputBlock, TruncationStage},
};
use ra_tools::read_file::{ReadFileLimits, ReadFileTool};
use serde_json::{Value, json};
use tempfile::TempDir;

/// Writes one fixture file and hands back the directory it lives in.
fn workspace(name: &str, contents: impl AsRef<[u8]>) -> TempDir {
    let dir = tempfile::tempdir().expect("a temporary workspace");
    std::fs::write(dir.path().join(name), contents).expect("the fixture file");
    dir
}

/// The run a direct call is made inside. `read_file` reads neither the run nor host state, but a
/// call always belongs to one, and the context is what says so.
fn run() -> RunContext {
    RunContext::new(
        RunId::new("run-read-file"),
        AgentSpec::builder()
            .id(AgentId::new("reader"))
            .name("Reader")
            .build()
            .expect("an agent"),
    )
}

async fn read(tool: &ReadFileTool, arguments: &Value) -> ra_core::error::Result<ToolOutput> {
    let call_id = CallId::new("call-1");
    let run = run();
    tool.call(ToolContext::new(&run, tool.origin(), &call_id, arguments))
        .await
}

/// Runs the call and, when it fails, the tool's own failure shaping — the path R3-4's dispatcher
/// takes for a tool declaring `ToolFailureHandling::Custom`.
async fn observe(tool: &ReadFileTool, arguments: &Value) -> ToolOutput {
    let call_id = CallId::new("call-1");
    let run = run();
    let context = || ToolContext::new(&run, tool.origin(), &call_id, arguments);
    match tool.call(context()).await {
        Ok(output) => output,
        Err(error) => tool
            .handle_failure(&context(), &error)
            .await
            .expect("failure shaping must not fail")
            .expect("read_file shapes every failure it produces"),
    }
}

fn rooted(dir: &TempDir) -> ReadFileTool {
    ReadFileTool::rooted(dir.path()).expect("a rooted read_file")
}

fn body(output: &ToolOutput) -> &str {
    output.as_text().expect("a single text block")
}

#[tokio::test]
async fn test_read_file_01() {
    let tool = ReadFileTool::new().expect("read_file builds");

    tool.validate().expect("identity and schema must agree");
    assert_eq!(tool.origin().qualified_name(), "read_file");
    assert!(tool.schema().strict_json_schema());

    let schema = tool.schema().input_schema();
    assert_eq!(schema["additionalProperties"], json!(false));
    // Strict mode requires *every* property in `required`, optional ones included; they express
    // optionality as a nullable union instead.
    assert_eq!(schema["required"], json!(["limit", "offset", "path"]));
}

#[tokio::test]
async fn test_read_file_02() {
    // R3-4b's batch shape reads this declaration to pick a read or a write lock. Three reads of
    // three files are one wall-clock read, and a read cannot observe another call's writes.
    let tool = ReadFileTool::new().expect("read_file builds");

    assert_eq!(tool.options().concurrency(), ToolConcurrency::Parallel);
    assert!(tool.options().is_advertised());

    // The repeat breaker is deliberately off here, and this assertion is what makes turning it on
    // a decision someone has to state. It compares arguments, and a re-read after an edit carries
    // the same arguments and different evidence — the tool this breaker would refuse first is the
    // one verifying that a write landed. Different arguments reset the streak, but requiring that
    // change merely to verify a write is still the wrong policy.
    assert_eq!(tool.options().max_repeat_streak(), None);
}

#[tokio::test]
async fn test_read_file_03() {
    let dir = workspace("hello.txt", "alpha\nbeta\n");
    let output = read(&rooted(&dir), &json!({ "path": "hello.txt" }))
        .await
        .expect("a readable file");

    assert_eq!(body(&output), "     1\talpha\n     2\tbeta\n");
    assert!(output.metadata().truncations().is_empty());
    assert!(output.metadata().guidance().is_empty());
    // The rendered projection is what a provider is sent. With nothing to say it must be the
    // blocks themselves, not a leading note that costs tokens on every later turn.
    assert_eq!(output.model_blocks(), output.blocks());
}

#[tokio::test]
async fn test_read_file_04() {
    let dir = workspace("hello.txt", "alpha\n");
    let output = read(&rooted(&dir), &json!({ "path": "hello.txt" }))
        .await
        .expect("a readable file");

    assert_eq!(body(&output), "     1\talpha\n");
}

#[tokio::test]
async fn test_read_file_05() {
    // No `limit` in the arguments: the 2-line ceiling is this tool's choice, so the model was
    // never told how much it did not receive. That is what makes it a truncation.
    let dir = workspace("many.txt", "a\nb\nc\nd\ne\n");
    let tool = rooted(&dir).with_limits(ReadFileLimits::new().with_default_line_limit(2));

    let output = read(&tool, &json!({ "path": "many.txt" }))
        .await
        .expect("a readable file");

    assert_eq!(body(&output), "     1\ta\n     2\tb\n");
    let truncations = output.metadata().truncations();
    assert_eq!(truncations.len(), 1);
    assert_eq!(truncations[0].stage(), TruncationStage::Tool);
    assert_eq!(truncations[0].original_bytes(), 10);
    assert_eq!(truncations[0].retained_bytes(), 4);
    assert_eq!(
        output.metadata().guidance(),
        ["Showing lines 1-2 of 5; continue from offset 3."]
    );
    // Rendering happens at the provider boundary and nowhere else: the note leads, the body is
    // untouched behind it.
    let rendered = output.model_blocks();
    assert_eq!(rendered.len(), 2);
    assert!(rendered[0].as_text().is_some_and(|note| note
        .contains("[truncated by tool: 4 of 10 bytes kept]")
        && note.contains("continue from offset 3")));
    assert_eq!(rendered[1], output.blocks()[0]);
}

#[tokio::test]
async fn test_read_file_06() {
    let dir = workspace("many.txt", "a\nb\nc\nd\ne\n");
    let output = read(
        &rooted(&dir),
        &json!({ "path": "many.txt", "offset": 2, "limit": 2 }),
    )
    .await
    .expect("a readable file");

    assert_eq!(body(&output), "     2\tb\n     3\tc\n");
    // It asked for two lines and got two lines. Calling that "truncated by tool" would make every
    // deliberate window look like a loss, and R5-1 reads these as data.
    assert!(output.metadata().truncations().is_empty());
    assert_eq!(
        output.metadata().guidance(),
        ["Showing lines 2-3 of 5; continue from offset 4."]
    );
}

#[tokio::test]
async fn test_read_file_07() {
    let dir = workspace("short.txt", "a\nb\n");
    let output = read(&rooted(&dir), &json!({ "path": "short.txt", "offset": 9 }))
        .await
        .expect("asking past the end is a well-formed question");

    assert_eq!(body(&output), "No lines at offset 9; the file has 2 lines.");
    assert_eq!(
        output.metadata().guidance(),
        ["Read again with an offset between 1 and 2."]
    );
}

#[tokio::test]
async fn test_read_file_08() {
    let dir = workspace("empty.txt", "");
    let output = read(&rooted(&dir), &json!({ "path": "empty.txt" }))
        .await
        .expect("an empty file is still a readable file");

    assert_eq!(body(&output), "The file is empty (0 bytes).");
    assert_eq!(output.blocks().len(), 1);
}

#[tokio::test]
async fn test_read_file_09() {
    let dir = workspace("wide.txt", "汉字汉字汉字\nshort\n");
    // Seven bytes lands mid-character on the third 3-byte char; the cut must fall back to six.
    let tool = rooted(&dir).with_limits(ReadFileLimits::new().with_max_line_bytes(7));

    let output = read(&tool, &json!({ "path": "wide.txt" }))
        .await
        .expect("a readable file");

    assert_eq!(body(&output), "     1\t汉字\n     2\tshort\n");
    let truncations = output.metadata().truncations();
    assert_eq!(truncations.len(), 1);
    assert_eq!(truncations[0].stage(), TruncationStage::Tool);
    assert_eq!(truncations[0].original_bytes(), 25);
    // The cut first line did not retain the source LF.  The display adds one for readability, but
    // truncation metadata is source-byte accounting rather than rendered-string accounting.
    assert_eq!(truncations[0].retained_bytes(), 12);
    assert_eq!(
        output.metadata().guidance(),
        ["1 line(s) over 7 bytes were cut to fit."]
    );
}

#[tokio::test]
async fn test_read_file_10() {
    let dir = workspace("many.txt", "alpha\nbeta\ngamma\n");
    let tool = rooted(&dir).with_limits(ReadFileLimits::new().with_max_output_bytes(1));

    let output = read(&tool, &json!({ "path": "many.txt" }))
        .await
        .expect("a readable file");

    // A body-less result says less than one over budget, so the first line is emitted whatever it
    // costs — and the truncation reports the overrun either way.
    assert_eq!(body(&output), "     1\talpha\n");
    assert_eq!(output.metadata().truncations().len(), 1);
    assert!(
        output
            .metadata()
            .guidance()
            .iter()
            .any(|line| line.contains("narrow the range with offset and limit"))
    );
}

#[tokio::test]
async fn test_read_file_11() {
    let dir = workspace("many.txt", "aaaaaaaa\nbbbbbbbb\ncccccccc\n");
    let tool = rooted(&dir).with_limits(
        ReadFileLimits::new()
            .with_default_line_limit(2)
            .with_max_line_bytes(4),
    );

    let output = read(&tool, &json!({ "path": "many.txt" }))
        .await
        .expect("a readable file");

    // The window cut and the rendering cut are separate facts about separate amounts. One slot
    // would let the second silently erase the first.
    let truncations = output.metadata().truncations();
    assert_eq!(truncations.len(), 2);
    assert_eq!(truncations[0].original_bytes(), 27);
    assert_eq!(truncations[0].retained_bytes(), 18);
    assert_eq!(truncations[1].original_bytes(), 18);
    assert_eq!(truncations[1].retained_bytes(), 8);
}

#[tokio::test]
async fn test_read_file_12() {
    let dir = workspace("windows.txt", b"a\r\nb\r\nc");
    let tool = rooted(&dir).with_limits(ReadFileLimits::new().with_default_line_limit(1));

    let output = read(&tool, &json!({ "path": "windows.txt" }))
        .await
        .expect("a readable CRLF file");

    assert_eq!(body(&output), "     1\ta\n");
    let truncation = &output.metadata().truncations()[0];
    // `a\r\n` is three source bytes, and the unshown `b\r\nc` is four more.  Counting display
    // lines as `line.len() + 1` would incorrectly report five and invent a final newline.
    assert_eq!(truncation.original_bytes(), 7);
    assert_eq!(truncation.retained_bytes(), 3);
}

#[tokio::test]
async fn test_read_file_13() {
    let dir = workspace("latin.txt", b"caf\xe9\nz\n");
    let tool = rooted(&dir).with_limits(
        ReadFileLimits::new()
            .with_default_line_limit(2)
            .with_max_line_bytes(3),
    );

    let output = read(&tool, &json!({ "path": "latin.txt" }))
        .await
        .expect("a Latin-1 source file is still readable");

    assert_eq!(body(&output), "     1\tcaf\n     2\tz\n");
    let truncation = &output.metadata().truncations()[0];
    assert_eq!(truncation.original_bytes(), 7);
    // `\xe9` was omitted; its U+FFFD display form must not turn that one source byte into three.
    assert_eq!(truncation.retained_bytes(), 5);
}

#[tokio::test]
async fn test_read_file_14() {
    let dir = tempfile::tempdir().expect("a temporary workspace");
    let mut file = std::fs::File::create(dir.path().join("large.txt")).expect("fixture opens");
    for _ in 0..256 {
        file.write_all(&[b'a'; 8 * 1024])
            .expect("fixture chunk writes");
    }
    file.write_all(b"\nb\n").expect("fixture tail writes");

    let tool = rooted(&dir).with_limits(ReadFileLimits::new().with_max_line_bytes(4));
    let output = read(&tool, &json!({ "path": "large.txt" }))
        .await
        .expect("the line is streamed rather than read wholesale");

    assert_eq!(body(&output), "     1\taaaa\n     2\tb\n");
    let truncation = &output.metadata().truncations()[0];
    assert_eq!(truncation.original_bytes(), (256 * 8 * 1024 + 3) as u64);
    assert_eq!(truncation.retained_bytes(), 6);
}

#[tokio::test]
async fn test_read_file_15() {
    let dir = workspace("small.txt", "ok\n");
    let tool = rooted(&dir).with_limits(ReadFileLimits::new().with_max_line_bytes(usize::MAX));

    let output = read(&tool, &json!({ "path": "small.txt" }))
        .await
        .expect("a huge configured limit must not reserve huge memory before reading");

    assert_eq!(body(&output), "     1\tok\n");
}

#[tokio::test]
async fn test_read_file_16() {
    let dir = workspace("latin.txt", b"caf\xe9\n");
    let output = read(&rooted(&dir), &json!({ "path": "latin.txt" }))
        .await
        .expect("a Latin-1 source file is still a source file");

    assert_eq!(body(&output), "     1\tcaf\u{fffd}\n");
    assert_eq!(
        output.metadata().guidance(),
        ["The file is not valid UTF-8; undecodable bytes were replaced."]
    );
}

#[tokio::test]
async fn test_read_file_17() {
    let dir = workspace("latin.txt", b"ok\n\xe9\n");
    let output = read(
        &rooted(&dir),
        &json!({ "path": "latin.txt", "offset": 1, "limit": 1 }),
    )
    .await
    .expect("a Latin-1 source file is still readable");

    assert_eq!(body(&output), "     1\tok\n");
    assert!(output
        .metadata()
        .guidance()
        .iter()
        .any(|guidance| guidance == "The file is not valid UTF-8; undecodable bytes were replaced."));
}

#[tokio::test]
async fn test_read_file_18() {
    // A one-pixel PNG. The point is the block kind and the media type, not the pixels.
    let png: &[u8] = &[
        0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, b'I', b'H', b'D',
        b'R',
    ];
    let dir = workspace("pixel.png", png);
    let output = read(&rooted(&dir), &json!({ "path": "pixel.png" }))
        .await
        .expect("a readable image");

    assert_eq!(output.blocks().len(), 1);
    let ToolOutputBlock::Image(image) = &output.blocks()[0] else {
        panic!("a .png must not come back as text");
    };
    let source = image.source().as_base64().expect("inline image bytes");
    assert_eq!(source.media_type(), "image/png");
    assert!(!source.data().is_empty());
}

#[tokio::test]
async fn test_read_file_19() {
    let dir = workspace("blob.bin", b"\x00\x01\x02binary");
    let output = observe(&rooted(&dir), &json!({ "path": "blob.bin" })).await;

    assert_eq!(body(&output), "`blob.bin` is binary, not text.");
    assert_eq!(
        output.metadata().guidance(),
        ["read_file returns text, images, and PDFs only."]
    );
}

#[tokio::test]
async fn test_read_file_20() {
    let dir = workspace("hello.txt", "alpha\n");
    let tool = rooted(&dir);

    // Left as an `Err`, because it is one: the dispatcher records the class, and only
    // `handle_failure` turns it into something the model can act on.
    let error = read(&tool, &json!({ "path": "missing.rs" }))
        .await
        .expect_err("a missing file is a failure, not an observation");
    assert_eq!(error.code(), "tool.invalid_input");

    let output = observe(&tool, &json!({ "path": "missing.rs" })).await;
    assert_eq!(body(&output), "No such file: `missing.rs`.");
    assert_eq!(
        output.metadata().guidance(),
        ["Check the path, or search for the file before reading it."]
    );
    // The sentence names the path the model sent, not the absolute path on this host: the host
    // layout is neither the model's business nor worth its tokens.
    assert!(!body(&output).contains(&dir.path().display().to_string()));
}

#[tokio::test]
async fn test_read_file_21() {
    let dir = workspace("hello.txt", "alpha\n");
    std::fs::create_dir(dir.path().join("src")).expect("a directory to point at");

    let output = observe(&rooted(&dir), &json!({ "path": "src" })).await;

    assert_eq!(body(&output), "`src` is not a regular file.");
}

#[tokio::test]
async fn test_read_file_22() {
    let dir = workspace("hello.txt", "alpha\n");

    // One path that exists on this host and one that does not, answered identically: the boundary
    // is decided lexically, before anything is opened, so the refusal cannot leak which is which.
    for escape in ["/etc/passwd", "/nowhere/at/all"] {
        let output = observe(&rooted(&dir), &json!({ "path": escape })).await;
        assert_eq!(
            body(&output),
            format!("`{escape}` is outside the workspace.")
        );
    }
}

#[tokio::test]
async fn test_read_file_23() {
    let dir = workspace("hello.txt", "alpha\n");
    std::fs::create_dir(dir.path().join("nested")).expect("a nested directory");

    // The first climbs out of the workspace and the second does not — and the answer is the same
    // for both, because the reason is the spelling rather than where it happens to land. Saying
    // "outside the workspace" for the second would be false, and would send the model looking for
    // a boundary problem instead of doing the one thing that fixes it.
    for requested in ["../../nowhere/at/all", "nested/../hello.txt"] {
        let output = observe(&rooted(&dir), &json!({ "path": requested })).await;
        assert_eq!(
            body(&output),
            format!("`{requested}` uses `..`, which this tool does not resolve.")
        );
        assert_eq!(output.metadata().guidance(), ["Send the path without `..`."]);
    }
}

#[tokio::test]
async fn test_read_file_24() {
    let dir = workspace("hello.txt", "alpha\n");
    let tool = rooted(&dir);
    // The canonical root, not `dir.path()`: on a host where the temporary directory is itself
    // reached through an alias, the two spell the same directory differently and only the
    // canonical one is what the boundary compares against.
    let absolute = tool
        .root()
        .expect("a rooted tool knows its root")
        .join("hello.txt");

    let output = read(
        &tool,
        &json!({ "path": absolute.to_str().expect("a UTF-8 fixture path") }),
    )
    .await
    .expect("an absolute path inside the workspace is the same request written another way");

    assert_eq!(body(&output), "     1\talpha\n");
}

#[cfg(unix)]
#[tokio::test]
async fn test_read_file_25() {
    let dir = workspace("hello.txt", "alpha\n");
    std::fs::create_dir(dir.path().join("nested")).expect("a nested directory");
    // Relative, which is what a repository stores. An absolute target is refused even when it
    // points back inside, because an absolute path means nothing relative to a directory handle.
    std::os::unix::fs::symlink("../hello.txt", dir.path().join("nested/link"))
        .expect("a symlink inside the workspace");

    let output = read(&rooted(&dir), &json!({ "path": "nested/link" }))
        .await
        .expect("a workspace whose paths are symlinked is an ordinary workspace");

    assert_eq!(body(&output), "     1\talpha\n");
}

#[cfg(unix)]
#[tokio::test]
async fn test_read_file_26() {
    let dir = workspace("hello.txt", "alpha\n");
    std::fs::create_dir(dir.path().join("nested")).expect("a nested directory");
    std::os::unix::fs::symlink("../somewhere", dir.path().join("nested/link"))
        .expect("a relative link");
    let requested = "nested/link/../hello.txt";

    let output = observe(&rooted(&dir), &json!({ "path": requested })).await;

    // POSIX resolves `..` after following a symlink, while lexical normalization would first turn
    // this into `nested/hello.txt`.  The capability boundary deliberately refuses this ambiguous
    // spelling rather than choosing a different workspace file on the model's behalf — and it says
    // so, because `nested/hello.txt` would have been a perfectly ordinary read.
    assert_eq!(
        body(&output),
        format!("`{requested}` uses `..`, which this tool does not resolve.")
    );
}

#[cfg(unix)]
#[tokio::test]
async fn test_read_file_27() {
    let secret = tempfile::tempdir().expect("a directory outside the workspace");
    std::fs::write(secret.path().join("id_rsa"), "PRIVATE KEY").expect("the secret");
    let dir = workspace("hello.txt", "alpha\n");
    // Lexical normalization cannot see this one; only canonicalizing before the second check can.
    std::os::unix::fs::symlink(secret.path().join("id_rsa"), dir.path().join("link"))
        .expect("a symlink out of the workspace");

    let output = observe(&rooted(&dir), &json!({ "path": "link" })).await;

    assert_eq!(body(&output), "`link` is outside the workspace.");
}

#[cfg(unix)]
#[tokio::test]
async fn test_read_file_28() {
    let parent = tempfile::tempdir().expect("a directory containing both fixtures");
    let root = parent.path().join("workspace");
    let secret = parent.path().join("secret");
    std::fs::create_dir(&root).expect("the workspace");
    std::fs::create_dir(&secret).expect("the outside directory");
    std::fs::write(secret.join("id_rsa"), "PRIVATE KEY").expect("the secret");
    std::os::unix::fs::symlink("../secret/id_rsa", root.join("link"))
        .expect("a relative link that escapes the workspace");

    let output = observe(
        &ReadFileTool::rooted(&root).expect("a rooted tool"),
        &json!({ "path": "link" }),
    )
    .await;

    assert_eq!(body(&output), "`link` is outside the workspace.");
}

#[tokio::test]
async fn test_read_file_29() {
    let tool = ReadFileTool::new().expect("read_file builds");
    assert!(tool.root().is_none());

    let output = read(&tool, &json!({ "path": "Cargo.toml" }))
        .await
        .expect("the test crate's own manifest");

    assert!(body(&output).contains("it-tools"));
}

#[tokio::test]
async fn test_read_file_30() {
    let dir = workspace("hello.txt", "alpha\n");
    let tool = rooted(&dir);
    let arguments = json!({ "path": "hello.txt", "recursive": true });

    let error = read(&tool, &arguments)
        .await
        .expect_err("an argument the schema does not declare");
    assert_eq!(error.code(), "tool.invalid_input");

    // A mistyped argument is the most correctable mistake a model makes. Under `Custom` failure
    // handling, a shaping gap here would propagate instead and stop the turn — so this one is
    // shaped too, and the sentence names the field the decoder rejected.
    let output = observe(&tool, &arguments).await;
    assert!(
        body(&output).starts_with("Invalid arguments:") && body(&output).contains("recursive"),
        "{}",
        body(&output)
    );
    assert_eq!(
        output.metadata().guidance(),
        ["Send `path`, and `offset` and `limit` only for text."]
    );
}

#[tokio::test]
async fn test_read_file_31() {
    // R2-10 gives all 15 entries 20 KB, and the measurement behind it is that the tools doing the
    // work are the cheap ones: Codex spends 1,635 B on `exec_command` and 554 B on `view_image`,
    // and keeps its budget for the orchestration tools. This is the ceiling for one advertised
    // entry, not a snapshot — a description that doubles should have to say so here.
    let tool = ReadFileTool::new().expect("read_file builds");
    let schema = tool.schema();
    let size = schema.canonical_json().expect("a renderable schema").len()
        + schema.name().len()
        + schema.description().map_or(0, str::len);

    assert!(size < 900, "read_file advertises {size} bytes");
}

#[tokio::test]
async fn test_read_file_32() {
    // R2-9's property, checked on the first real tool: Codex's 16 schemas were byte-identical
    // across 82 requests, and a schema that jitters is a cache prefix that never hits.
    let first = ReadFileTool::new()
        .expect("read_file builds")
        .schema()
        .canonical_json()
        .expect("a renderable schema");

    for _ in 0..100 {
        let again = ReadFileTool::new()
            .expect("read_file builds")
            .schema()
            .canonical_json()
            .expect("a renderable schema");
        assert_eq!(again, first);
    }
}
