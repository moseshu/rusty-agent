//! `ra-tools::sandbox::skills` and `ra-sandbox::skills` against a session backed by a host
//! directory.
//!
//! Ported from the reference's `tests/sandbox/capabilities/test_skills_capability.py`, in upstream
//! order. The session is the counterpart of the reference's `_SkillsSession`: every path is
//! measured from a real directory standing in for the workspace, and the user of every read, write
//! and `mkdir`, and every command, is recorded. The reference's `scripted_sandbox_session` is the
//! same session over a root nothing is written under.
//!
//! The reference's pydantic constructor keywords are [`Skills::builder`] here, and its
//! `instructions(manifest)` is [`Skills::instructions_for`]. Its `bind`, `bind_run_as` and
//! `bind_workspace_scope` are [`Skills::bound_to`], [`Skills::with_run_as`] and
//! [`Skills::with_workspace_scope`].

#[path = "../../it-sandbox/tests/support/span_log.rs"]
mod span_log;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ra_core::{
    agent::AgentSpec,
    capability::{Capability, SandboxBinding},
    context::RunContext,
    item::{AgentId, CallId},
    sandbox::{
        AsUser, Entry, EntryKind, ErrorCode, ExecRequest, ExecResult, FileEntry, FileMode, Group,
        LazySkillSource, Manifest, NO_SKILL_DESCRIPTION, Permissions, SandboxError,
        SandboxPathGrant, SandboxResult, SandboxSession, SandboxSessionState,
        SandboxWorkspaceScope, SessionPath, SessionResources, SkillLoadResult, SkillMetadata,
        Snapshot, User, parse_skill_frontmatter,
    },
    state::RunId,
    tool::{ToolApprovalPolicy, ToolConcurrency, ToolContext},
};
use ra_sandbox::instrumentation::InstrumentedSession;
use ra_sandbox::materialize::ManifestApplier;
use ra_sandbox::skills::LocalDirLazySkillSource;
use ra_tools::sandbox::skills::{
    DEFAULT_SKILLS_PATH, LOAD_SKILL_DESCRIPTION, LOAD_SKILL_TOOL_NAME, Skill, Skills,
};
use serde_json::{Value, json};
use span_log::SpanLog;

const EAGER_INDEX: &str = include_str!("fixtures/sandbox_skills/eager_index.md");
const LAZY_SCOPED_INDEX: &str = include_str!("fixtures/sandbox_skills/lazy_scoped_index.md");

// ---- the host-backed session ---------------------------------------------------------------

#[derive(Default)]
struct Record {
    read_users: Vec<Option<String>>,
    write_users: Vec<Option<String>>,
    mkdir_users: Vec<Option<String>>,
    commands: Vec<Vec<String>>,
}

/// How the session answers a read.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Reads {
    /// From the directory.
    Host,
    /// Always with an archive read failure, as `_ArchiveReadErrorSkillsSession`.
    ArchiveError,
}

struct HostSession {
    state: SandboxSessionState,
    resources: SessionResources,
    reads: Reads,
    record: Mutex<Record>,
}

impl HostSession {
    fn new(manifest: Manifest) -> Arc<Self> {
        Self::reading(manifest, Reads::Host)
    }

    fn reading(manifest: Manifest, reads: Reads) -> Arc<Self> {
        Arc::new(Self {
            state: SandboxSessionState::new("skills", Snapshot::noop(), manifest),
            resources: SessionResources::new(),
            reads,
            record: Mutex::new(Record::default()),
        })
    }

    /// The reference's `normalize_path`: the host path a workspace path lands on.
    fn host_path(&self, path: &str) -> SandboxResult<PathBuf> {
        Ok(PathBuf::from(
            self.workspace_path_policy()?
                .normalize_sandbox_path(path, false)?
                .as_str(),
        ))
    }

    fn with<T>(&self, read: impl FnOnce(&Record) -> T) -> T {
        read(&self.record.lock().unwrap())
    }
}

fn user_name(user: &AsUser) -> Option<String> {
    user.as_ref().map(|user| user.name.clone())
}

fn io_failure(path: &Path, error: &std::io::Error) -> SandboxError {
    if error.kind() == std::io::ErrorKind::NotFound {
        SandboxError::workspace_read_not_found(&path.to_string_lossy())
    } else {
        SandboxError::workspace_archive_read(&path.to_string_lossy())
    }
}

#[async_trait]
impl SandboxSession for HostSession {
    fn backend_id(&self) -> &str {
        "skills"
    }

    fn state(&self) -> SandboxSessionState {
        self.state.clone()
    }

    fn resources(&self) -> &SessionResources {
        &self.resources
    }

    async fn exec(&self, request: ExecRequest) -> SandboxResult<ExecResult> {
        self.record
            .lock()
            .unwrap()
            .commands
            .push(request.command().to_vec());
        Ok(ExecResult::new(Vec::new(), Vec::new(), 0))
    }

    async fn running(&self) -> SandboxResult<bool> {
        Ok(true)
    }

    async fn ls(&self, path: SessionPath<'_>, _user: AsUser) -> SandboxResult<Vec<FileEntry>> {
        let path = path.as_str();
        let directory = self.host_path(path)?;
        let listing =
            std::fs::read_dir(&directory).map_err(|error| io_failure(&directory, &error))?;
        let mut children: Vec<PathBuf> = listing.map(|child| child.unwrap().path()).collect();
        children.sort();
        Ok(children
            .into_iter()
            .map(|child| {
                let kind = if child.is_dir() {
                    EntryKind::Directory
                } else {
                    EntryKind::File
                };
                FileEntry::new(child.to_string_lossy(), Permissions::default()).with_kind(kind)
            })
            .collect())
    }

    async fn rm(
        &self,
        _path: SessionPath<'_>,
        _recursive: bool,
        _user: AsUser,
    ) -> SandboxResult<()> {
        panic!("the skills capability never removes anything");
    }

    async fn mkdir(
        &self,
        path: SessionPath<'_>,
        _parents: bool,
        user: AsUser,
    ) -> SandboxResult<()> {
        let path = path.as_str();
        self.record
            .lock()
            .unwrap()
            .mkdir_users
            .push(user_name(&user));
        let directory = self.host_path(path)?;
        std::fs::create_dir_all(&directory).map_err(|error| io_failure(&directory, &error))
    }

    async fn read(&self, path: SessionPath<'_>, user: AsUser) -> SandboxResult<Vec<u8>> {
        let path = path.as_str();
        self.record
            .lock()
            .unwrap()
            .read_users
            .push(user_name(&user));
        let file = self.host_path(path)?;
        if self.reads == Reads::ArchiveError {
            return Err(SandboxError::workspace_archive_read(
                &file.to_string_lossy(),
            ));
        }
        std::fs::read(&file).map_err(|error| io_failure(&file, &error))
    }

    async fn write(&self, path: SessionPath<'_>, data: Vec<u8>, user: AsUser) -> SandboxResult<()> {
        let path = path.as_str();
        self.record
            .lock()
            .unwrap()
            .write_users
            .push(user_name(&user));
        let file = self.host_path(path)?;
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, data).map_err(|error| io_failure(&file, &error))
    }

    async fn persist_workspace(&self) -> SandboxResult<Vec<u8>> {
        Ok(Vec::new())
    }

    async fn hydrate_workspace(&self, _data: Vec<u8>) -> SandboxResult<()> {
        Ok(())
    }
}

// ---- helpers --------------------------------------------------------------------------------

/// A temporary directory, by its resolved path: grants are resolved before they are compared.
struct Scratch {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Scratch {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        Self { _dir: dir, root }
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }

    /// A workspace directory, created.
    fn workspace(&self) -> PathBuf {
        let workspace = self.path("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        workspace
    }

    /// `skills/<name>/SKILL.md` holding `markdown`; answers the source root.
    fn skill(&self, name: &str, markdown: &str) -> PathBuf {
        let source = self.path("skills");
        std::fs::create_dir_all(source.join(name)).unwrap();
        std::fs::write(source.join(name).join("SKILL.md"), markdown).unwrap();
        source
    }
}

fn text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn grant(source: &Path) -> SandboxPathGrant {
    SandboxPathGrant::new(&text(source)).unwrap()
}

/// The reference's `_source_granted_manifest`.
fn source_granted_manifest(root: &str, source: &Path) -> Manifest {
    Manifest::new()
        .with_root(root)
        .with_path_grant(grant(source))
}

fn lazy_local_dir(source: &Path) -> LocalDirLazySkillSource {
    LocalDirLazySkillSource::new(Entry::local_dir(Some(text(source)))).unwrap()
}

fn literal(name: &str, description: &str, content: &str) -> Skill {
    Skill::new(name, description, content).unwrap()
}

fn skills_of(skills: Vec<Skill>) -> Skills {
    Skills::builder().skills(skills).build().unwrap()
}

fn lazy_skills(source: impl LazySkillSource) -> Skills {
    Skills::builder().lazy_from(source).build().unwrap()
}

fn scope(cwd: &str) -> SandboxWorkspaceScope {
    SandboxWorkspaceScope::from_cwd(Some(cwd)).unwrap()
}

fn workspace_manifest() -> Manifest {
    Manifest::new().with_root("/workspace")
}

fn processed(capability: &Skills, mut manifest: Manifest) -> SandboxResult<Manifest> {
    capability.process_manifest(&mut manifest)?;
    Ok(manifest)
}

fn children_keys(entry: &Entry) -> Vec<&str> {
    entry
        .children()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect()
}

fn context(error: &SandboxError, key: &str) -> Option<Value> {
    error.context().get(key).cloned()
}

fn run_context() -> RunContext {
    let agent = AgentSpec::builder()
        .id(AgentId::new("skills"))
        .name("Skills")
        .build()
        .unwrap();
    RunContext::new(RunId::new("run-sandbox-skills"), &agent)
}

/// Calls the capability's only tool through its `Tool` entry, as the runtime would.
async fn invoke_load_skill(capability: &Skills, arguments: &Value) -> String {
    let tools = capability.try_tools().unwrap();
    let tool = tools[0].as_ref();
    let run = run_context();
    let call_id = CallId::new("call");
    tool.call(ToolContext::new(&run, tool, &call_id, arguments))
        .await
        .unwrap()
        .as_text()
        .unwrap()
        .to_owned()
}

/// A lazy source that lists at most one skill and answers every load with a fixed result.
struct StaticResultLazySkillSource {
    result: SkillLoadResult,
    metadata_path: Option<&'static str>,
}

impl StaticResultLazySkillSource {
    fn answering(result: SkillLoadResult) -> Self {
        Self {
            result,
            metadata_path: None,
        }
    }
}

#[async_trait]
impl LazySkillSource for StaticResultLazySkillSource {
    fn list_skill_metadata(
        &self,
        _skills_path: &str,
        _source_grants: &[SandboxPathGrant],
    ) -> SandboxResult<Vec<SkillMetadata>> {
        Ok(self
            .metadata_path
            .map(|path| SkillMetadata::new("dynamic-skill", "dynamic description", path))
            .into_iter()
            .collect())
    }

    async fn load_skill(
        &self,
        _skill_name: &str,
        _session: &Arc<dyn SandboxSession>,
        _skills_path: &str,
        _user: Option<&User>,
    ) -> SandboxResult<SkillLoadResult> {
        Ok(self.result.clone())
    }
}

fn loaded(status: &str, skill_name: &str, path: &str) -> SkillLoadResult {
    SkillLoadResult::loaded(status, skill_name, path)
}

// ---- TestSkillValidation --------------------------------------------------------------------

#[test]
fn a_skill_whose_content_is_a_directory_is_refused() {
    let error = Skill::with_content_entry("my-skill", "desc", Entry::dir()).unwrap_err();

    assert_eq!(error.error_code(), ErrorCode::SkillsConfigInvalid);
    assert_eq!(error.message(), "skill content must be file-like");
    assert_eq!(context(&error, "content_type"), Some(json!("dir")));
}

#[test]
fn script_paths_that_normalize_to_one_path_are_refused() {
    let error = literal("my-skill", "desc", "literal")
        .with_script("run.sh", Entry::file("echo one"))
        .unwrap()
        .with_script("./run.sh", Entry::file("echo two"))
        .unwrap_err();

    assert_eq!(error.error_code(), ErrorCode::SkillsConfigInvalid);
    assert_eq!(error.message(), "duplicate entry path in skill scripts");
    assert_eq!(context(&error, "entry_path"), Some(json!("run.sh")));
}

// ---- TestSkillsValidation -------------------------------------------------------------------

#[test]
fn a_capability_needs_a_source() {
    let error = Skills::builder().build().unwrap_err();

    assert_eq!(error.error_code(), ErrorCode::SkillsConfigInvalid);
    assert_eq!(
        error.message(),
        "skills capability requires `skills`, `from_`, or `lazy_from`"
    );
}

#[test]
fn an_entry_source_that_is_not_a_directory_is_refused() {
    let error = Skills::builder()
        .from_entry(Entry::file("not-a-dir"))
        .build()
        .unwrap_err();

    assert_eq!(error.message(), "`from_` must be a directory-like artifact");
    assert_eq!(context(&error, "artifact_type"), Some(json!("file")));
}

#[test]
fn two_skills_of_one_name_are_refused() {
    let error = Skills::builder()
        .skill(literal("dup", "first", "a"))
        .skill(literal("dup", "second", "b"))
        .build()
        .unwrap_err();

    assert_eq!(error.message(), "duplicate skill name: dup");
    assert_eq!(context(&error, "field"), Some(json!("skills[].name")));
}

#[test]
fn literal_skills_and_an_entry_source_are_not_combined() {
    let error = Skills::builder()
        .from_entry(Entry::dir().with_child(
            "my-skill",
            Entry::dir().with_child("SKILL.md", Entry::file("imported")),
        ))
        .skill(literal("my-skill", "desc", "literal"))
        .build()
        .unwrap_err();

    assert_eq!(
        error.message(),
        "skills capability accepts only one of `skills`, `from_`, or `lazy_from`"
    );
    assert_eq!(context(&error, "has_from"), Some(json!(true)));
}

#[test]
fn literal_skills_and_a_lazy_source_are_not_combined() {
    let error = Skills::builder()
        .skill(literal("my-skill", "desc", "literal"))
        .lazy_from(lazy_local_dir(Path::new("skills")))
        .build()
        .unwrap_err();

    assert_eq!(
        error.message(),
        "skills capability accepts only one of `skills`, `from_`, or `lazy_from`"
    );
    assert_eq!(context(&error, "has_from"), Some(json!(false)));
}

#[test]
fn an_absolute_skills_path_is_refused() {
    let error = Skills::builder()
        .skill(literal("my-skill", "desc", "literal"))
        .skills_path("/skills")
        .build()
        .unwrap_err();

    assert_eq!(error.message(), "skills_path must be a relative path");
    assert_eq!(context(&error, "reason"), Some(json!("absolute")));
}

#[test]
fn a_windows_drive_skills_path_is_refused_as_absolute() {
    let error = Skills::builder()
        .skill(literal("my-skill", "desc", "literal"))
        .skills_path("C:\\skills")
        .build()
        .unwrap_err();

    let context: Vec<(String, Value)> = error
        .context()
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    assert_eq!(
        context,
        vec![
            ("field".to_owned(), json!("skills_path")),
            ("path".to_owned(), json!("C:/skills")),
            ("reason".to_owned(), json!("absolute")),
        ]
    );
}

#[test]
fn a_skills_path_climbing_out_is_refused() {
    let error = Skills::builder()
        .skill(literal("my-skill", "desc", "literal"))
        .skills_path("../skills")
        .build()
        .unwrap_err();

    assert_eq!(
        error.message(),
        "skills_path must not escape the skills root"
    );
    assert_eq!(context(&error, "reason"), Some(json!("escape_root")));
}

// ---- TestSkillsManifest ---------------------------------------------------------------------

#[test]
fn a_literal_skill_is_written_with_its_whole_structure() {
    let skill = literal("my-skill", "desc", "Use this skill.")
        .with_script("run.sh", Entry::file("echo run"))
        .unwrap()
        .with_reference("docs/readme.md", Entry::file("ref"))
        .unwrap()
        .with_asset("images/icon.txt", Entry::file("asset"))
        .unwrap();
    let capability = skills_of(vec![skill]);

    let manifest = processed(&capability, workspace_manifest()).unwrap();
    let skill_entry = &manifest.entries[".agents/my-skill"];

    assert_eq!(
        children_keys(skill_entry),
        ["SKILL.md", "assets", "references", "scripts"]
    );
    let children = skill_entry.children().unwrap();
    assert_eq!(children_keys(&children["scripts"]), ["run.sh"]);
    assert_eq!(children_keys(&children["references"]), ["docs/readme.md"]);
    assert_eq!(children_keys(&children["assets"]), ["images/icon.txt"]);
}

#[test]
fn an_entry_source_is_placed_at_the_skills_path() {
    let source = Entry::dir().with_child(
        "imported",
        Entry::dir().with_child("SKILL.md", Entry::file("imported")),
    );
    let capability = Skills::builder()
        .from_entry(source.clone())
        .build()
        .unwrap();

    let manifest = processed(&capability, workspace_manifest()).unwrap();

    assert_eq!(manifest.entries[".agents"], source);
}

#[test]
fn a_host_directory_source_stays_eager_unless_asked() {
    let scratch = Scratch::new();
    let source = scratch.skill("dynamic-skill", "# Skill\n");
    let capability = Skills::builder()
        .from_entry(Entry::local_dir(Some(text(&source))))
        .build()
        .unwrap();

    let manifest = processed(&capability, workspace_manifest()).unwrap();

    assert_eq!(manifest.entries[".agents"].entry_type(), "local_dir");
}

#[test]
fn a_lazy_source_adds_nothing_to_the_manifest() {
    let scratch = Scratch::new();
    let source = scratch.skill("dynamic-skill", "# Skill\n");
    let capability = lazy_skills(lazy_local_dir(&source));

    let manifest = processed(&capability, workspace_manifest()).unwrap();

    assert!(manifest.entries.is_empty());
}

#[test]
fn a_lazy_source_refuses_entries_overlapping_its_path() {
    let scratch = Scratch::new();
    let source = scratch.skill("dynamic-skill", "# Skill\n");
    let capability = lazy_skills(lazy_local_dir(&source));

    let error = processed(
        &capability,
        workspace_manifest().with_entry(".agents", Entry::dir()),
    )
    .unwrap_err();

    assert_eq!(
        error.message(),
        "skills lazy_from path overlaps existing manifest entries"
    );
    let context: Vec<(String, Value)> = error
        .context()
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    assert_eq!(
        context,
        vec![
            ("overlaps".to_owned(), json!([".agents"])),
            ("path".to_owned(), json!(".agents")),
            ("source".to_owned(), json!("lazy_from")),
        ]
    );
}

#[test]
fn a_literal_skill_already_in_the_manifest_as_it_would_be_written_is_kept() {
    let skill = literal("my-skill", "desc", "Use this skill.")
        .with_script("run.sh", Entry::file("echo run"))
        .unwrap();
    let rendered = skill.as_dir_entry();
    let capability = skills_of(vec![skill]);
    let manifest = workspace_manifest().with_entry(".agents/my-skill", rendered.clone());

    let processed = processed(&capability, manifest.clone()).unwrap();

    assert_eq!(processed, manifest);
    assert_eq!(processed.entries[".agents/my-skill"], rendered);
}

#[test]
fn a_literal_skill_colliding_with_another_entry_is_refused() {
    let capability = skills_of(vec![literal("my-skill", "desc", "literal")]);

    let error = processed(
        &capability,
        workspace_manifest().with_entry(".agents/my-skill", Entry::dir()),
    )
    .unwrap_err();

    assert_eq!(error.error_code(), ErrorCode::SkillsConfigInvalid);
    assert_eq!(error.message(), "skill path already exists in manifest");
}

#[test]
fn a_custom_skills_path_is_where_skills_are_written() {
    let capability = Skills::builder()
        .skill(literal("my-skill", "desc", "literal"))
        .skills_path(".sandbox/skills")
        .build()
        .unwrap();

    let manifest = processed(&capability, workspace_manifest()).unwrap();

    assert_eq!(
        manifest.entries[".sandbox/skills/my-skill"],
        capability.skills()[0].as_dir_entry()
    );
}

// ---- TestSkillsInstructions -----------------------------------------------------------------

#[tokio::test]
async fn the_index_lists_literal_skills_by_name_under_the_root() {
    let capability = skills_of(vec![
        literal("z-skill", "z description", "z"),
        literal("a-skill", "a description", "a"),
    ]);

    let instructions = capability
        .instructions_for(&workspace_manifest())
        .await
        .unwrap()
        .unwrap();

    assert!(instructions.starts_with("## Skills\n"));
    assert!(instructions.contains("### Available skills"));
    assert!(instructions.contains("### How to use skills"));
    assert!(!instructions.contains("### Run-scoped skill paths"));
    let a_line = instructions
        .find("- a-skill: a description (file: .agents/a-skill)")
        .unwrap();
    let z_line = instructions
        .find("- z-skill: z description (file: .agents/z-skill)")
        .unwrap();
    assert!(a_line < z_line);
    // Byte for byte what the reference renders for the same configuration.
    assert_eq!(instructions, EAGER_INDEX);
}

#[tokio::test]
async fn the_index_uses_a_custom_skills_path() {
    let capability = Skills::builder()
        .skill(literal("my-skill", "desc", "literal"))
        .skills_path(".sandbox/skills")
        .build()
        .unwrap();

    let instructions = capability
        .instructions_for(&workspace_manifest())
        .await
        .unwrap()
        .unwrap();

    assert!(instructions.contains("- my-skill: desc (file: .sandbox/skills/my-skill)"));
}

#[tokio::test]
async fn with_a_run_working_directory_session_skills_are_listed_absolutely() {
    let capability = skills_of(vec![literal("my-skill", "desc", "literal")])
        .with_workspace_scope(scope("tasks/task-a"));

    let instructions = capability
        .instructions_for(&workspace_manifest())
        .await
        .unwrap()
        .unwrap();

    assert!(instructions.contains("- my-skill: desc (file: /workspace/.agents/my-skill)"));
    assert!(instructions.contains("Treat each listed path as the skill root"));
    assert!(instructions.contains("write task inputs, outputs, caches, and temporary files"));
}

#[tokio::test]
async fn no_skills_means_no_index() {
    let capability = Skills::builder().from_entry(Entry::dir()).build().unwrap();

    let instructions = capability
        .instructions_for(&workspace_manifest())
        .await
        .unwrap();

    assert_eq!(instructions, None);
}

#[tokio::test]
async fn a_lazy_host_directory_is_indexed_only_under_a_grant() {
    let scratch = Scratch::new();
    let source = scratch.skill(
        "dynamic-skill",
        "---\nname: hidden-skill\ndescription: outside base\n---\n# Skill\n",
    );
    let capability = lazy_skills(lazy_local_dir(&source));

    let instructions = capability
        .instructions_for(&workspace_manifest())
        .await
        .unwrap();

    assert_eq!(instructions, None);
}

#[tokio::test]
async fn an_entry_source_is_indexed_from_the_frontmatter_in_the_workspace() {
    let scratch = Scratch::new();
    let workspace = scratch.workspace();
    let capability = Skills::builder()
        .from_entry(Entry::dir().with_child(
            "dynamic-skill",
            Entry::dir().with_child(
                "SKILL.md",
                Entry::file(
                    "---\nname: discovered-skill\ndescription: loaded from runtime \
                     frontmatter\n---\n\n# Skill\n",
                ),
            ),
        ))
        .build()
        .unwrap();
    let manifest = processed(&capability, Manifest::new().with_root(text(&workspace))).unwrap();
    let session = HostSession::new(manifest.clone());
    ManifestApplier::new(session.clone(), PathBuf::from("."))
        .apply_manifest(&manifest, false)
        .await
        .unwrap();
    let capability = capability
        .bound_to(session.clone())
        .with_workspace_scope(scope("tasks/task-a"));

    let instructions = capability
        .instructions_for(&session.state().manifest().clone())
        .await
        .unwrap()
        .unwrap();

    assert!(instructions.contains(&format!(
        "- discovered-skill: loaded from runtime frontmatter (file: {}/.agents/dynamic-skill)",
        text(&workspace)
    )));
}

#[tokio::test]
async fn a_granted_lazy_host_directory_is_indexed_from_its_frontmatter() {
    let scratch = Scratch::new();
    let source = scratch.skill(
        "dynamic-skill",
        "---\nname: discovered-skill\ndescription: local dir metadata\n---\n# Skill\n",
    );
    let capability = lazy_skills(lazy_local_dir(&source));

    assert_eq!(
        capability
            .instructions_for(&workspace_manifest())
            .await
            .unwrap(),
        None
    );

    let instructions = capability
        .instructions_for(&source_granted_manifest("/workspace", &source))
        .await
        .unwrap()
        .unwrap();

    assert!(
        instructions
            .contains("- discovered-skill: local dir metadata (file: .agents/dynamic-skill)")
    );
    assert!(instructions.contains("Call `load_skill` with a single skill name from the list"));
    assert!(instructions.contains("loaded on demand instead of being present up front"));
}

#[tokio::test]
async fn a_symlinked_skill_directory_is_not_indexed() {
    let scratch = Scratch::new();
    let source = scratch.path("skills");
    let outside_skill = scratch.path("outside/linked-skill");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::create_dir_all(&outside_skill).unwrap();
    std::fs::write(
        outside_skill.join("SKILL.md"),
        "---\nname: linked-skill\ndescription: linked metadata\n---\n# Skill\n",
    )
    .unwrap();
    std::os::unix::fs::symlink(&outside_skill, source.join("linked-skill")).unwrap();
    let capability = lazy_skills(lazy_local_dir(&source));

    let instructions = capability
        .instructions_for(&source_granted_manifest("/workspace", &source))
        .await
        .unwrap();

    assert_eq!(instructions, None);
}

#[tokio::test]
async fn load_skill_stages_exactly_the_skill_asked_for() {
    let scratch = Scratch::new();
    let workspace = scratch.workspace();
    let source = scratch.skill("dynamic-skill", "# dynamic skill\n");
    let capability = lazy_skills(lazy_local_dir(&source));
    let manifest = processed(
        &capability,
        source_granted_manifest(&text(&workspace), &source),
    )
    .unwrap();
    assert!(manifest.entries.is_empty());
    let session = HostSession::new(manifest);
    let capability = capability.bound_to(session.clone());

    let missing = session
        .read(".agents/dynamic-skill/SKILL.md".into(), None)
        .await
        .unwrap_err();
    assert_eq!(missing.error_code(), ErrorCode::WorkspaceReadNotFound);

    let output = invoke_load_skill(&capability, &json!({"skill_name": "dynamic-skill"})).await;

    assert_eq!(
        serde_json::from_str::<Value>(&output).unwrap(),
        json!({
            "status": "loaded",
            "skill_name": "dynamic-skill",
            "path": ".agents/dynamic-skill",
        })
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join(".agents/dynamic-skill/SKILL.md")).unwrap(),
        "# dynamic skill\n"
    );
}

#[tokio::test]
async fn a_loaded_skill_is_reported_absolutely_and_staged_under_the_root() {
    let scratch = Scratch::new();
    let workspace = scratch.workspace();
    let source = scratch.skill("dynamic-skill", "# dynamic skill\n");
    let capability = lazy_skills(lazy_local_dir(&source));
    let session = HostSession::new(
        processed(
            &capability,
            source_granted_manifest(&text(&workspace), &source),
        )
        .unwrap(),
    );
    let capability = capability
        .bound_to(session)
        .with_workspace_scope(scope("tasks/task-a"));

    let output = capability.load_skill("dynamic-skill").await.unwrap();

    assert_eq!(
        output,
        loaded(
            "loaded",
            "dynamic-skill",
            &format!("{}/.agents/dynamic-skill", text(&workspace))
        )
    );
    assert!(workspace.join(".agents/dynamic-skill/SKILL.md").is_file());
    assert!(!workspace.join("tasks/task-a/.agents").exists());
}

#[tokio::test]
async fn a_loaded_skill_gets_the_source_entry_s_permissions_and_group() {
    let scratch = Scratch::new();
    let workspace = scratch.workspace();
    let source_root = scratch.skill("dynamic-skill", "# dynamic skill\n");
    let source = Entry::local_dir(Some(text(&source_root)))
        .with_permissions(
            Permissions::default()
                .owner_can(FileMode::All)
                .group_can(FileMode::None)
                .others_can(FileMode::None),
        )
        .owned_by(ra_core::sandbox::EntryOwner::Group(Group::new(
            "staff",
            Vec::new(),
        )));
    let lazy_source = LocalDirLazySkillSource::new(source.clone()).unwrap();
    let capability = lazy_skills(lazy_source.clone());
    let session = HostSession::new(
        processed(
            &capability,
            source_granted_manifest(&text(&workspace), &source_root),
        )
        .unwrap(),
    );
    let capability = capability.bound_to(session.clone());

    invoke_load_skill(&capability, &json!({"skill_name": "dynamic-skill"})).await;

    let skill_dest = text(&workspace.join(".agents/dynamic-skill"));
    let commands = session.with(|record| record.commands.clone());
    assert!(commands.contains(&vec![
        "chmod".to_owned(),
        "0700".to_owned(),
        skill_dest.clone()
    ]));
    assert!(commands.contains(&vec!["chgrp".to_owned(), "staff".to_owned(), skill_dest]));
    // The configured source is not repointed at the loaded skill.
    assert_eq!(lazy_source.source(), &source);
}

#[tokio::test]
async fn a_loaded_skill_keeps_the_default_permissions() {
    let scratch = Scratch::new();
    let workspace = scratch.workspace();
    let source = scratch.skill("dynamic-skill", "# dynamic skill\n");
    let capability = lazy_skills(lazy_local_dir(&source));
    let session = HostSession::new(
        processed(
            &capability,
            source_granted_manifest(&text(&workspace), &source),
        )
        .unwrap(),
    );
    let capability = capability.bound_to(session.clone());

    invoke_load_skill(&capability, &json!({"skill_name": "dynamic-skill"})).await;

    let skill_dest = text(&workspace.join(".agents/dynamic-skill"));
    let commands = session.with(|record| record.commands.clone());
    assert!(commands.contains(&vec!["chmod".to_owned(), "0755".to_owned(), skill_dest]));
    assert!(!commands.iter().any(|command| command[0] == "chgrp"));
}

// ---- TestSkillsLazyLoading ------------------------------------------------------------------

#[tokio::test]
async fn a_custom_source_s_result_passes_through_without_a_working_directory() {
    let expected = SkillLoadResult::new()
        .with_field("status", "loaded")
        .with_field("detail", "opaque");
    let capability = lazy_skills(StaticResultLazySkillSource::answering(expected.clone()))
        .bound_to(HostSession::new(workspace_manifest()));

    let output = capability.load_skill("dynamic-skill").await.unwrap();

    assert_eq!(output, expected);
}

#[rstest::rstest]
#[case::missing(SkillLoadResult::new().with_field("status", "loaded"), "missing")]
#[case::escaping(loaded_with_path("../escape"), "invalid")]
#[case::backslashed(loaded_with_path(".agents\\dynamic-skill"), "invalid")]
#[tokio::test]
async fn under_a_working_directory_a_custom_source_must_report_a_workspace_path(
    #[case] result: SkillLoadResult,
    #[case] reason: &str,
) {
    let capability = lazy_skills(StaticResultLazySkillSource::answering(result))
        .bound_to(HostSession::new(workspace_manifest()))
        .with_workspace_scope(scope("tasks/task-a"));

    let error = capability.load_skill("dynamic-skill").await.unwrap_err();

    assert_eq!(
        error.message(),
        "skill path must be non-empty and workspace-relative when sandbox.cwd is configured"
    );
    assert_eq!(context(&error, "skill_name"), Some(json!("dynamic-skill")));
    assert_eq!(context(&error, "field"), Some(json!("path")));
    assert_eq!(context(&error, "reason"), Some(json!(reason)));
}

fn loaded_with_path(path: &str) -> SkillLoadResult {
    SkillLoadResult::new()
        .with_field("status", "loaded")
        .with_field("path", path)
}

#[rstest::rstest]
#[case::posix("../outside")]
#[case::windows("..\\outside")]
#[tokio::test]
async fn an_indexed_path_that_is_not_workspace_relative_is_a_configuration_error(
    #[case] metadata_path: &'static str,
) {
    let capability = lazy_skills(StaticResultLazySkillSource {
        result: loaded_with_path(".agents/dynamic-skill"),
        metadata_path: Some(metadata_path),
    })
    .with_workspace_scope(scope("tasks/task-a"));

    let error = capability
        .instructions_for(&workspace_manifest())
        .await
        .unwrap_err();

    assert_eq!(
        error.message(),
        "skill path must be non-empty and workspace-relative when sandbox.cwd is configured"
    );
    let context: Vec<(String, Value)> = error
        .context()
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    assert_eq!(
        context,
        vec![
            ("field".to_owned(), json!("path")),
            ("path".to_owned(), json!("../outside")),
            ("reason".to_owned(), json!("invalid")),
            ("skill_name".to_owned(), json!("dynamic-skill")),
        ]
    );
    assert!(std::error::Error::source(&error).is_some());
}

#[test]
fn without_a_lazy_source_there_are_no_tools() {
    let capability = skills_of(vec![literal("my-skill", "desc", "literal")]);

    assert!(capability.try_tools().unwrap().is_empty());
}

#[test]
fn a_lazy_source_needs_a_bound_session_for_its_tool() {
    let scratch = Scratch::new();
    let source = scratch.skill("dynamic-skill", "# Skill\n");
    let capability = lazy_skills(lazy_local_dir(&source));

    let error = capability.try_tools().err().unwrap();

    assert!(
        error
            .to_string()
            .contains("Skills is not bound to a SandboxSession")
    );
}

#[test]
fn a_bound_lazy_source_offers_load_skill() {
    let scratch = Scratch::new();
    let workspace = scratch.workspace();
    let source = scratch.skill("dynamic-skill", "# Skill\n");
    let capability = lazy_skills(lazy_local_dir(&source)).bound_to(HostSession::new(
        source_granted_manifest(&text(&workspace), &source),
    ));

    let tools = capability.try_tools().unwrap();

    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].schema().name(), "load_skill");
}

#[tokio::test]
async fn load_skill_is_refused_without_a_lazy_source() {
    let capability = skills_of(vec![literal("my-skill", "desc", "literal")]);

    let error = capability.load_skill("my-skill").await.unwrap_err();

    assert_eq!(error.error_code(), ErrorCode::SkillsConfigInvalid);
    assert_eq!(
        error.message(),
        "load_skill is only available when lazy_from is configured"
    );
}

#[tokio::test]
async fn a_skill_already_in_the_workspace_is_reported_as_already_loaded() {
    let scratch = Scratch::new();
    let workspace = scratch.workspace();
    let source = scratch.skill("dynamic-skill", "# dynamic skill\n");
    let session = HostSession::new(source_granted_manifest(&text(&workspace), &source));
    let capability = lazy_skills(lazy_local_dir(&source)).bound_to(session.clone());
    session
        .write(
            ".agents/dynamic-skill/SKILL.md".into(),
            b"# already loaded\n".to_vec(),
            None,
        )
        .await
        .unwrap();

    let output = capability.load_skill("dynamic-skill").await.unwrap();

    assert_eq!(
        output,
        loaded("already_loaded", "dynamic-skill", ".agents/dynamic-skill")
    );
}

#[tokio::test]
async fn a_skill_is_probed_and_staged_as_the_bound_user() {
    let scratch = Scratch::new();
    let workspace = scratch.workspace();
    let source = scratch.skill("dynamic-skill", "# dynamic skill\n");
    let session = HostSession::new(source_granted_manifest(&text(&workspace), &source));
    let capability = lazy_skills(lazy_local_dir(&source))
        .bound_to(session.clone())
        .with_run_as(Some(User::new("sandbox-user")));

    let output = capability.load_skill("dynamic-skill").await.unwrap();

    assert_eq!(
        output,
        loaded("loaded", "dynamic-skill", ".agents/dynamic-skill")
    );
    let sandbox_user = Some("sandbox-user".to_owned());
    session.with(|record| {
        assert_eq!(record.read_users, vec![sandbox_user.clone()]);
        assert_eq!(record.write_users, vec![sandbox_user.clone()]);
        assert!(!record.mkdir_users.is_empty());
        assert!(record.mkdir_users.iter().all(|user| *user == sandbox_user));
        // Staged as a user, so the entry's own ownership and mode are left alone.
        assert!(record.commands.is_empty());
    });
}

/// The reference runs this for a session that raises `FileNotFoundError` and for one that raises
/// `WorkspaceReadNotFoundError`; both are `workspace_read_not_found` here, so there is one case.
#[tokio::test]
async fn a_first_load_s_probe_is_not_a_span_error() {
    let scratch = Scratch::new();
    let workspace = scratch.workspace();
    let source = scratch.skill("dynamic-skill", "# dynamic skill\n");
    let inner = HostSession::new(source_granted_manifest(&text(&workspace), &source));
    let session: Arc<dyn SandboxSession> =
        Arc::new(InstrumentedSession::new(inner, None, None).unwrap());
    let capability = lazy_skills(lazy_local_dir(&source)).bound_to(session);
    let (log, _guard) = SpanLog::install();

    let output = capability.load_skill("dynamic-skill").await.unwrap();

    assert_eq!(
        output,
        loaded("loaded", "dynamic-skill", ".agents/dynamic-skill")
    );
    let reads: Vec<_> = log
        .sandbox_spans()
        .into_iter()
        .filter(|span| span.kind() == "sandbox.read")
        .collect();
    assert_eq!(reads.len(), 1);
    assert_eq!(reads[0].field("outcome"), Some("ok"));
    assert_eq!(reads[0].field("error.type"), None);
}

#[tokio::test]
async fn a_probe_failing_otherwise_stops_the_load_and_marks_the_span() {
    let scratch = Scratch::new();
    let workspace = scratch.workspace();
    let source = scratch.skill("dynamic-skill", "# dynamic skill\n");
    let inner = HostSession::reading(
        source_granted_manifest(&text(&workspace), &source),
        Reads::ArchiveError,
    );
    let session: Arc<dyn SandboxSession> =
        Arc::new(InstrumentedSession::new(inner.clone(), None, None).unwrap());
    let capability = lazy_skills(lazy_local_dir(&source)).bound_to(session);
    let (log, _guard) = SpanLog::install();

    let error = capability.load_skill("dynamic-skill").await.unwrap_err();

    assert_eq!(error.error_code(), ErrorCode::WorkspaceArchiveReadError);
    inner.with(|record| {
        assert!(record.write_users.is_empty());
        assert!(record.mkdir_users.is_empty());
    });
    let reads: Vec<_> = log
        .sandbox_spans()
        .into_iter()
        .filter(|span| span.kind() == "sandbox.read")
        .collect();
    assert_eq!(reads.len(), 1);
    assert_eq!(reads[0].field("outcome"), Some("error"));
}

#[tokio::test]
async fn a_missing_lazy_source_directory_is_a_configuration_error() {
    let scratch = Scratch::new();
    let workspace = scratch.workspace();
    let missing = scratch.path("missing-skills");
    let capability = lazy_skills(lazy_local_dir(&missing)).bound_to(HostSession::new(
        source_granted_manifest(&text(&workspace), &missing),
    ));

    let error = capability.load_skill("missing-skill").await.unwrap_err();

    assert_eq!(error.error_code(), ErrorCode::SkillsConfigInvalid);
    assert_eq!(
        error.message(),
        "lazy skill source directory is unavailable"
    );
}

#[tokio::test]
async fn a_name_two_skills_share_is_ambiguous() {
    let scratch = Scratch::new();
    let workspace = scratch.workspace();
    scratch.skill(
        "skill-one",
        "---\nname: shared-skill\ndescription: first\n---\n# Skill\n",
    );
    let source = scratch.skill(
        "skill-two",
        "---\nname: shared-skill\ndescription: second\n---\n# Skill\n",
    );
    let capability = lazy_skills(lazy_local_dir(&source)).bound_to(HostSession::new(
        source_granted_manifest(&text(&workspace), &source),
    ));

    let error = capability.load_skill("shared-skill").await.unwrap_err();

    assert_eq!(error.error_code(), ErrorCode::SkillsConfigInvalid);
    assert_eq!(error.message(), "lazy skill name is ambiguous");
    assert_eq!(
        context(&error, "matching_paths"),
        Some(json!([".agents/skill-one", ".agents/skill-two"]))
    );
}

#[tokio::test]
async fn binding_again_forgets_the_cached_index() {
    let scratch = Scratch::new();
    let workspace = scratch.workspace();
    let source = scratch.skill(
        "dynamic-skill",
        "---\nname: cached-skill\ndescription: old description\n---\n# Skill\n",
    );
    let manifest = source_granted_manifest(&text(&workspace), &source);
    let capability = lazy_skills(lazy_local_dir(&source));

    let first = capability
        .instructions_for(&manifest)
        .await
        .unwrap()
        .unwrap();
    std::fs::write(
        source.join("dynamic-skill/SKILL.md"),
        "---\nname: cached-skill\ndescription: new description\n---\n# Skill\n",
    )
    .unwrap();
    let second = capability
        .instructions_for(&manifest)
        .await
        .unwrap()
        .unwrap();
    let rebound = capability.bound_to(HostSession::new(manifest.clone()));
    let third = rebound.instructions_for(&manifest).await.unwrap().unwrap();

    assert!(first.contains("- cached-skill: old description (file: .agents/dynamic-skill)"));
    assert!(second.contains("- cached-skill: old description (file: .agents/dynamic-skill)"));
    assert!(third.contains("- cached-skill: new description (file: .agents/dynamic-skill)"));
}

#[tokio::test]
async fn a_changed_grant_source_invalidates_the_cached_index() {
    let scratch = Scratch::new();
    let source = scratch.skill(
        "dynamic-skill",
        "---\nname: cached-skill\ndescription: cached description\n---\n# Skill\n",
    );
    let other_root = scratch.path("other-skills");
    std::fs::create_dir_all(&other_root).unwrap();
    let capability = lazy_skills(lazy_local_dir(&source));
    let granted_from = |host: &Path| {
        workspace_manifest().with_path_grant(
            SandboxPathGrant::new("/mnt/skills")
                .unwrap()
                .with_host_path(&text(host))
                .unwrap(),
        )
    };

    let first = capability
        .instructions_for(&granted_from(&source))
        .await
        .unwrap();
    let second = capability
        .instructions_for(&granted_from(&other_root))
        .await
        .unwrap();

    assert!(
        first
            .unwrap()
            .contains("- cached-skill: cached description (file: .agents/dynamic-skill)")
    );
    assert_eq!(second, None);
}

// ---- beyond the reference's tests -----------------------------------------------------------

/// A lazy source listing the one skill the fixture was rendered for.
struct FixtureSkillSource;

#[async_trait]
impl LazySkillSource for FixtureSkillSource {
    fn list_skill_metadata(
        &self,
        skills_path: &str,
        _source_grants: &[SandboxPathGrant],
    ) -> SandboxResult<Vec<SkillMetadata>> {
        Ok(vec![SkillMetadata::new(
            "lazy-skill",
            "lazy description",
            &format!("{skills_path}/lazy-skill"),
        )])
    }

    async fn load_skill(
        &self,
        _skill_name: &str,
        _session: &Arc<dyn SandboxSession>,
        _skills_path: &str,
        _user: Option<&User>,
    ) -> SandboxResult<SkillLoadResult> {
        Ok(SkillLoadResult::new())
    }
}

#[tokio::test]
async fn the_lazy_index_under_a_working_directory_is_the_reference_s() {
    let capability = lazy_skills(FixtureSkillSource).with_workspace_scope(scope("tasks/task-a"));

    let instructions = capability
        .instructions_for(&workspace_manifest())
        .await
        .unwrap()
        .unwrap();

    assert_eq!(instructions, LAZY_SCOPED_INDEX);
}

#[test]
fn frontmatter_is_read_as_the_reference_reads_it() {
    let cases: [(&str, &[(&str, &str)]); 7] = [
        (
            "---\nname: a\ndescription: 'quoted: yes'\n---\nbody",
            &[("description", "quoted: yes"), ("name", "a")],
        ),
        (
            "---\r\nname: crlf\r\n# comment\r\nno colon\r\n  key :  \"v\" \r\n---",
            &[("key", "v"), ("name", "crlf")],
        ),
        ("no front\n---\nname: x\n---", &[]),
        ("---\nname: unterminated", &[]),
        ("  ---  \nname: x\nname: y\n  ---  ", &[("name", "y")]),
        ("---\nk: '\n---", &[("k", "'")]),
        ("---\u{2028}name: sep\u{2028}---", &[("name", "sep")]),
    ];
    for (markdown, expected) in cases {
        let parsed = parse_skill_frontmatter(markdown);
        let parsed: Vec<(&str, &str)> = parsed
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
            .collect();
        assert_eq!(parsed, expected, "{markdown:?}");
    }
}

#[test]
fn load_skill_is_parallel_needs_no_approval_and_has_the_reference_s_schema() {
    let scratch = Scratch::new();
    let workspace = scratch.workspace();
    let source = scratch.skill("dynamic-skill", "# Skill\n");
    let capability = lazy_skills(lazy_local_dir(&source)).bound_to(HostSession::new(
        source_granted_manifest(&text(&workspace), &source),
    ));
    let tools = capability.try_tools().unwrap();
    let tool = tools[0].as_ref();

    assert_eq!(tool.options().approval(), ToolApprovalPolicy::Never);
    assert_eq!(tool.options().concurrency(), ToolConcurrency::Parallel);
    let schema = tool.schema();
    assert_eq!(schema.name(), LOAD_SKILL_TOOL_NAME);
    assert_eq!(schema.description(), Some(LOAD_SKILL_DESCRIPTION));
    assert!(!schema.strict_json_schema());
    let input = schema.input_schema();
    assert_eq!(input["required"], json!(["skill_name"]));
    assert_eq!(input["properties"]["skill_name"]["type"], json!("string"));
    assert_eq!(input["properties"]["skill_name"].get("description"), None);
}

#[test]
fn a_lazy_source_must_be_a_host_directory() {
    let error = LocalDirLazySkillSource::new(Entry::file("not a directory")).unwrap_err();

    assert_eq!(error.error_code(), ErrorCode::SkillsConfigInvalid);
    assert_eq!(context(&error, "source_type"), Some(json!("file")));
}

#[test]
fn a_skill_directory_without_frontmatter_is_named_after_itself() {
    let scratch = Scratch::new();
    let source = scratch.skill("plain-skill", "# no frontmatter\n");
    std::fs::write(source.join("not-a-skill.txt"), "ignored").unwrap();
    std::fs::create_dir_all(source.join("no-skill-md")).unwrap();

    let metadata = lazy_local_dir(&source)
        .list_skill_metadata(DEFAULT_SKILLS_PATH, &[grant(&source)])
        .unwrap();

    assert_eq!(
        metadata,
        vec![SkillMetadata::new(
            "plain-skill",
            NO_SKILL_DESCRIPTION,
            ".agents/plain-skill"
        )]
    );
}

#[tokio::test]
async fn invalid_utf8_fails_the_lazy_index_and_load_without_caching_or_writing() {
    let scratch = Scratch::new();
    let workspace = scratch.workspace();
    let source = scratch.skill("broken", "# placeholder");
    let skill_md = source.join("broken/SKILL.md");
    std::fs::write(&skill_md, b"# invalid\n\xff").unwrap();
    let manifest = source_granted_manifest(&text(&workspace), &source);
    let session = HostSession::new(manifest.clone());
    let capability = lazy_skills(lazy_local_dir(&source)).bound_to(session.clone());

    let index_error = capability.instructions_for(&manifest).await.unwrap_err();
    let load_error = capability.load_skill("broken").await.unwrap_err();
    for error in [&index_error, &load_error] {
        assert_eq!(error.error_code(), ErrorCode::SkillsConfigInvalid);
        assert_eq!(context(error, "path"), Some(json!(text(&skill_md))));
        assert!(
            std::error::Error::source(error)
                .unwrap()
                .downcast_ref::<std::string::FromUtf8Error>()
                .is_some()
        );
    }
    session.with(|record| {
        assert!(record.read_users.is_empty());
        assert!(record.write_users.is_empty());
        assert!(record.mkdir_users.is_empty());
    });

    std::fs::write(&skill_md, "# repaired").unwrap();
    assert!(
        capability
            .instructions_for(&manifest)
            .await
            .unwrap()
            .unwrap()
            .contains("- broken:")
    );
    assert_eq!(
        capability.load_skill("broken").await.unwrap(),
        loaded("loaded", "broken", ".agents/broken")
    );
}

#[tokio::test]
async fn a_bound_capability_renders_its_index_from_the_binding_s_manifest() {
    let session = HostSession::new(workspace_manifest());
    let capability = skills_of(vec![literal("my-skill", "desc", "literal")]);
    let binding = SandboxBinding::new(session, None, scope("tasks/task-a"), workspace_manifest());

    let bound = capability.bind_sandbox(&binding).unwrap().unwrap();
    let section = bound.instructions().await.unwrap().unwrap();

    assert!(
        section
            .content()
            .contains("- my-skill: desc (file: /workspace/.agents/my-skill)")
    );
    assert_eq!(capability.instructions().await.unwrap(), None);
}

#[test]
fn an_unbound_capability_is_refused_on_the_run_configuration() {
    let capability = skills_of(vec![literal("my-skill", "desc", "literal")]);

    let error = capability.bind(&run_context()).err().unwrap();

    assert!(
        error
            .to_string()
            .contains("Skills is not bound to a SandboxSession")
    );
}

#[tokio::test]
async fn load_skill_answers_with_the_source_s_fields_in_order() {
    let result = SkillLoadResult::new()
        .with_field("status", "loaded")
        .with_field("detail", "opaque")
        .with_field("path", "somewhere");
    let capability = lazy_skills(StaticResultLazySkillSource::answering(result))
        .bound_to(HostSession::new(workspace_manifest()));

    let output = invoke_load_skill(&capability, &json!({"skill_name": "x", "extra": 1})).await;

    assert_eq!(
        output,
        r#"{"status":"loaded","detail":"opaque","path":"somewhere"}"#
    );
}
