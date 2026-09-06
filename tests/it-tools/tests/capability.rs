//! The built-in capabilities: what each one contributes, and what it deliberately does not.

use std::sync::Arc;

use async_trait::async_trait;
use ra_core::{
    agent::AgentSpec,
    capability::{Capability, CapabilityFamily},
    context::RunContext,
    error::Result,
    item::{AgentId, CallId},
    memory::{
        MemoryExcerpt, MemoryHit, MemoryHits, MemoryListRequest, MemoryListing, MemoryReadRequest,
        MemoryRecord, MemoryRecordId, MemoryRecordKind, MemorySearchRequest, MemoryStore,
    },
    model::ModelSettings,
    permission::PermissionScope,
    state::RunId,
    tool::{Tool, ToolContext, ToolOutput},
};
use ra_core::{
    skill::{SkillCatalog, SkillDocument, SkillId, SkillSummary},
    web::{WebAccess, WebDocument, WebFetchRequest, WebSearchRequest, WebSearchResults},
};
use ra_exec::{fs::Workspace, session::ProcessManager};
use ra_tools::{
    capability::{
        ApplyPatchCapability, FilesystemCapability, MemoryCapability, SearchCapability,
        ShellCapability, SkillsCapability, TodoCapability, ViewImageCapability, WebCapability,
    },
    skill::SkillListingLimits,
};
use serde_json::json;
use tempfile::TempDir;

fn workspace(directory: &TempDir) -> Workspace {
    Workspace::open(directory.path()).expect("workspace opens")
}

/// A store holding one record, whose text a test chooses.
///
/// The text is a parameter because one case turns on it: the memory fragment must be the same
/// whatever the store holds, and a store with no contents to vary could not show that.
struct OneRecordStore {
    text: &'static str,
}

impl OneRecordStore {
    fn id() -> MemoryRecordId {
        MemoryRecordId::new("m1")
    }
}

#[async_trait]
impl MemoryStore for OneRecordStore {
    async fn list(&self, _request: MemoryListRequest) -> Result<MemoryListing> {
        Ok(MemoryListing::new(vec![MemoryRecord::new(
            Self::id(),
            "MEMORY.md",
            MemoryRecordKind::Record,
        )]))
    }

    async fn read(&self, request: MemoryReadRequest) -> Result<MemoryExcerpt> {
        Ok(MemoryExcerpt::new(
            request.record().clone(),
            "MEMORY.md",
            self.text,
        ))
    }

    async fn search(&self, _request: MemorySearchRequest) -> Result<MemoryHits> {
        Ok(MemoryHits::new(vec![MemoryHit::new(
            Self::id(),
            "MEMORY.md",
            self.text,
        )]))
    }
}

fn memory_capability(text: &'static str) -> MemoryCapability {
    MemoryCapability::new(Arc::new(OneRecordStore { text })).expect("memory builds")
}

/// A backend that answers both web operations from nothing.
///
/// The web family's contract is what these cases are about — what it contributes, what its entries
/// may do, what its fragment says — and none of that depends on a backend answering usefully. What
/// a real one returns is exercised where the entries themselves are.
struct EmptyWeb;

#[async_trait]
impl WebAccess for EmptyWeb {
    async fn search(&self, _request: WebSearchRequest) -> Result<WebSearchResults> {
        Ok(WebSearchResults::default())
    }

    async fn fetch(&self, request: WebFetchRequest) -> Result<WebDocument> {
        Ok(WebDocument::new(request.address(), ""))
    }
}

fn web_capability() -> WebCapability {
    WebCapability::new(Arc::new(EmptyWeb)).expect("web builds")
}

/// A catalog holding one skill, whose description a test chooses.
///
/// The description is a parameter because one case turns on it: unlike memory's, this family's
/// fragment is *supposed* to vary with what the catalog holds, and a catalog with nothing to vary
/// could not show it.
struct OneSkillCatalog {
    description: &'static str,
}

#[async_trait]
impl SkillCatalog for OneSkillCatalog {
    async fn list(&self) -> Result<Vec<SkillSummary>> {
        Ok(vec![SkillSummary::new(
            SkillId::new("release"),
            "Release checklist",
            self.description,
        )])
    }

    async fn load(&self, skill: &SkillId) -> Result<SkillDocument> {
        Ok(SkillDocument::new(skill.clone(), "Release checklist", "1."))
    }
}

fn skills_capability(description: &'static str) -> SkillsCapability {
    SkillsCapability::new(Arc::new(OneSkillCatalog { description })).expect("skills builds")
}

/// The nine capabilities this crate backs, built over one workspace and one backend of each kind.
fn built_in(workspace: &Workspace) -> Vec<Box<dyn Capability>> {
    vec![
        Box::new(FilesystemCapability::for_workspace(workspace).expect("filesystem builds")),
        Box::new(SearchCapability::for_workspace(workspace).expect("search builds")),
        Box::new(ViewImageCapability::for_workspace(workspace).expect("view_image builds")),
        Box::new(TodoCapability::new().expect("todo builds")),
        Box::new(ApplyPatchCapability::for_workspace(workspace).expect("apply_patch builds")),
        Box::new(
            ShellCapability::for_workspace(workspace, Arc::new(ProcessManager::default()))
                .expect("shell builds"),
        ),
        Box::new(memory_capability("the workspace pins its toolchain")),
        Box::new(web_capability()),
        Box::new(skills_capability("How this team ships.")),
    ]
}

fn advertised_names(capability: &dyn Capability) -> Vec<String> {
    capability
        .tools()
        .iter()
        .map(|tool| tool.origin().qualified_name().to_owned())
        .collect()
}

async fn call(tool: &Arc<dyn Tool>, arguments: serde_json::Value) -> ToolOutput {
    let agent = AgentSpec::builder()
        .id(AgentId::new("runner"))
        .name("Runner")
        .build()
        .expect("an agent");
    let run = RunContext::new(RunId::new("run-capability"), agent.as_ref());
    tool.call(ToolContext::new(
        &run,
        tool.as_ref(),
        &CallId::new("call-capability"),
        &arguments,
    ))
    .await
    .expect("the tool call succeeds")
}

/// Each capability answers for its own family and contributes exactly the entries it names.
///
/// The pairing is the assertion. A capability whose `kind` and `tools` drift apart is what makes a
/// dependency on a family satisfiable by something that does not serve it.
#[test]
fn test_each_built_in_capability_contributes_its_own_entries() {
    let directory = tempfile::tempdir().expect("workspace directory");
    let workspace = workspace(&directory);

    let filesystem = FilesystemCapability::for_workspace(&workspace).expect("filesystem builds");
    let search = SearchCapability::for_workspace(&workspace).expect("search builds");
    let view_image = ViewImageCapability::for_workspace(&workspace).expect("view_image builds");
    let todo = TodoCapability::new().expect("todo builds");
    let apply_patch = ApplyPatchCapability::for_workspace(&workspace).expect("apply_patch builds");
    let shell = ShellCapability::for_workspace(&workspace, Arc::new(ProcessManager::default()))
        .expect("shell builds");
    let memory = memory_capability("anything");
    let web = web_capability();
    let skills = skills_capability("anything");

    assert_eq!(filesystem.kind(), CapabilityFamily::FILESYSTEM);
    assert_eq!(advertised_names(&filesystem), ["read_file"]);
    assert_eq!(search.kind(), CapabilityFamily::SEARCH);
    assert_eq!(advertised_names(&search), ["grep", "glob"]);
    assert_eq!(view_image.kind(), CapabilityFamily::VIEW_IMAGE);
    assert_eq!(advertised_names(&view_image), ["view_image"]);
    assert_eq!(todo.kind(), CapabilityFamily::TODO);
    assert_eq!(advertised_names(&todo), ["update_plan"]);
    assert_eq!(apply_patch.kind(), CapabilityFamily::APPLY_PATCH);
    assert_eq!(advertised_names(&apply_patch), ["apply_patch"]);
    assert_eq!(shell.kind(), CapabilityFamily::SHELL);
    assert_eq!(advertised_names(&shell), ["exec_command", "write_stdin"]);
    assert_eq!(memory.kind(), CapabilityFamily::MEMORY);
    assert_eq!(
        advertised_names(&memory),
        ["memory_search", "memory_read", "memory_list"]
    );
    assert_eq!(web.kind(), CapabilityFamily::WEB);
    assert_eq!(advertised_names(&web), ["web_search", "web_fetch"]);
    assert_eq!(skills.kind(), CapabilityFamily::SKILLS);
    assert_eq!(advertised_names(&skills), ["skill"]);
}

/// The observing families observe, and the two that change things say so.
///
/// This is what lets a read-only surface be selected by installing capabilities rather than by
/// filtering a capability's tools — a split that only holds while every tool inside one family
/// agrees about what it can do.
#[test]
fn test_a_capability_is_uniform_in_what_its_entries_may_do() {
    let directory = tempfile::tempdir().expect("workspace directory");
    let workspace = workspace(&directory);

    let observing: Vec<Box<dyn Capability>> = vec![
        Box::new(FilesystemCapability::for_workspace(&workspace).expect("filesystem builds")),
        Box::new(SearchCapability::for_workspace(&workspace).expect("search builds")),
        Box::new(ViewImageCapability::for_workspace(&workspace).expect("view_image builds")),
        Box::new(TodoCapability::new().expect("todo builds")),
        Box::new(memory_capability("anything")),
        Box::new(skills_capability("anything")),
    ];
    for capability in &observing {
        for tool in capability.tools() {
            assert_eq!(
                tool.options().permission_scope(),
                PermissionScope::Read,
                "`{}` contributes `{}`, which is not read-only",
                capability.kind(),
                tool.origin().qualified_name()
            );
        }
    }

    // `web` is here rather than above, and it is the case that shows what the split is about. Its
    // entries change nothing in the workspace — but the scope excludes invoking an external action,
    // and a lookup sends the query out and brings a third party's text back. A role that withholds
    // everything with a side effect means to withhold this one too.
    let changing: Vec<Box<dyn Capability>> = vec![
        Box::new(ApplyPatchCapability::for_workspace(&workspace).expect("apply_patch builds")),
        Box::new(
            ShellCapability::for_workspace(&workspace, Arc::new(ProcessManager::default()))
                .expect("shell builds"),
        ),
        Box::new(web_capability()),
    ];
    for capability in &changing {
        for tool in capability.tools() {
            assert_ne!(
                tool.options().permission_scope(),
                PermissionScope::Read,
                "`{}` contributes `{}`, which would survive a read-only filter",
                capability.kind(),
                tool.origin().qualified_name()
            );
        }
    }
}

/// The shell pair addresses one session manager, because the capability handed both the same one.
///
/// This is the reason the two entries are one capability rather than two installations. The test
/// starts a background command through the capability's own `exec_command` and answers its prompt
/// through the capability's own `write_stdin`: a mismatched pair would report an unknown session
/// instead of the reply below.
#[tokio::test]
async fn test_the_shell_pair_shares_the_session_manager_it_was_built_with() {
    let directory = tempfile::tempdir().expect("workspace directory");
    let process_manager = Arc::new(ProcessManager::default());
    let shell =
        ShellCapability::for_workspace(&workspace(&directory), Arc::clone(&process_manager))
            .expect("shell builds");

    let started = call(
        &shell.exec_command(),
        json!({
            "cmd": "read line; printf 'reply:%s\\n' \"$line\"; sleep 30",
            "workdir": null,
            "shell": null,
            "tty": null,
            "login": null,
            "yield_time_ms": 100,
            "timeout_ms": null
        }),
    )
    .await;
    assert!(
        started.as_text().is_some(),
        "the command yields a model-visible result"
    );

    let session_id = process_manager
        .active_sessions()
        .await
        .into_iter()
        .next()
        .expect("the capability's exec_command started a session on the shared manager");
    let answered = call(
        &shell.write_stdin(),
        json!({
            "session_id": session_id.to_string(),
            "chars": "hello from the pair\n",
            "yield_time_ms": 1_000
        }),
    )
    .await;

    let text = answered.as_text().expect("text output");
    assert!(
        text.contains("reply:hello from the pair"),
        "the pair did not address one session: {text}"
    );

    process_manager.cancel(&session_id).await;
}

/// None of the built-in five requires another, so a host may install any subset.
///
/// A dependency is a hard assembly error and an ordering edge, not documentation of a habit.
/// Declaring `apply_patch` -> `filesystem` because editing usually follows reading would refuse a
/// configuration that reads through the shell, which is a configuration that works.
///
/// **Memory is in this list on purpose.** It was expected to bring the first real edge, on the
/// reasoning that a memory capability cannot read its own store without a family that reaches the
/// filesystem. That is true of the arrangement where memory only contributes prompt text and the
/// model reads the store with `read_file` or a shell command; this one brings its own three
/// entries, so the edge is gone because the dependency is. If a later change routes memory through
/// the workspace tools again, this assertion is what says so.
#[test]
fn test_the_built_in_capabilities_require_nothing_of_each_other() {
    let directory = tempfile::tempdir().expect("workspace directory");

    for capability in &built_in(&workspace(&directory)) {
        assert!(
            capability.required_capabilities().is_empty(),
            "`{}` declares a dependency",
            capability.kind()
        );
    }
}

/// A tool capability contributes tools and the text describing them, and nothing else.
///
/// Sampling settings and the context-processor chain belong to whoever has a reason to change them.
/// A capability that silently narrowed the sampling layer, or inserted itself into the processor
/// chain, would make installing an entry a decision about the model call as well.
#[tokio::test]
async fn test_a_tool_capability_leaves_sampling_and_the_processor_chain_alone() {
    let directory = tempfile::tempdir().expect("workspace directory");

    let capabilities = built_in(&workspace(&directory));
    let settings = ModelSettings::new().with_temperature(0.25);
    for capability in &capabilities {
        assert_eq!(
            capability.sampling_params(settings.clone()),
            settings,
            "`{}` folded the sampling layer",
            capability.kind()
        );
        assert!(
            capability.context_processor().is_none(),
            "`{}` installed a context transform",
            capability.kind()
        );
    }
}

/// Each capability's fragment is one section, named and attributed to its own family.
///
/// The three properties are what assembly checks, and they are checked here as well because the
/// failure they prevent is silent from the capability's side: a fragment landing on another
/// section's name replaces text nobody will miss, and one attributed elsewhere reports the wrong
/// author in a dump that exists to say who wrote what.
#[tokio::test]
async fn test_each_built_in_capability_describes_its_own_entries_in_the_cached_prefix() {
    let directory = tempfile::tempdir().expect("workspace directory");

    for capability in &built_in(&workspace(&directory)) {
        let family = capability.kind();
        let section = capability
            .static_instructions()
            .await
            .expect("a built-in fragment resolves")
            .unwrap_or_else(|| panic!("`{family}` contributes no prompt fragment"));

        assert_eq!(section.name(), &family.prompt_section_name());
        assert_eq!(section.source(), &family.prompt_source());
        assert!(section.position().is_prefix(), "`{family}`");
        assert!(section.stability().is_stable(), "`{family}`");

        // A fragment in the cached prefix is paid for on every turn of every run, so it declares
        // what it may cost — and the declaration is only worth having if it is met.
        let budget = section
            .token_budget()
            .unwrap_or_else(|| panic!("`{family}` spends prefix tokens without declaring a share"));
        assert!(
            section.token_estimate() <= budget,
            "`{family}` spends {} of its declared {budget}",
            section.token_estimate()
        );

        // Every entry the capability contributes is named by the text that describes it. This is
        // the half a product cannot check: it can compare the fragment against the surface, but
        // only the capability knows an entry exists at all.
        for tool in capability.tools() {
            let name = tool.model_definition().name().to_owned();
            assert!(
                section.content().contains(&format!("`{name}`")),
                "`{family}` describes its entries without naming `{name}`: {}",
                section.content()
            );
        }
    }
}

/// The memory fragment is the same whatever the store holds.
///
/// This is R10-8's hard constraint made structural. The fragment lands in the cached prefix, which
/// is one span shared by every run of an agent and read from cache on every turn. Text derived from
/// the store would move that span whenever memory changed; text derived from a *query* would move it
/// every turn, which is the arrangement that turns a cache read into a full-price prefix on every
/// call. What the store holds reaches the model through tool results, in the tail, where varying
/// content costs what it costs and nothing else.
///
/// Two stores whose contents differ, one fragment: a capability that had read its store to write
/// the paragraph would fail here rather than in production, where the symptom is a cache hit rate
/// and not an error.
#[tokio::test]
async fn test_the_memory_fragment_does_not_vary_with_what_the_store_holds() {
    let one = memory_capability("the workspace pins its toolchain")
        .static_instructions()
        .await
        .expect("the fragment resolves")
        .expect("memory contributes a fragment");
    let other = memory_capability("nothing of the kind, and rather longer than the first")
        .static_instructions()
        .await
        .expect("the fragment resolves")
        .expect("memory contributes a fragment");

    assert_eq!(
        one.content(),
        other.content(),
        "the memory fragment is a function of its store, so the cached prefix moves with memory"
    );
    assert!(
        !one.content().contains("toolchain"),
        "the fragment quotes the store: {}",
        one.content()
    );
}

/// Everything a memory entry returns leaves through a tool result, never through the prefix.
///
/// The companion to the case above, from the other end: the fragment is constant *and* the store's
/// contents do reach the model, so the constraint is that they arrive in the tail rather than that
/// they never arrive. A memory surface that satisfied the first without the second would be a
/// capability whose entries return nothing.
#[tokio::test]
async fn test_what_a_memory_entry_returns_reaches_the_model_as_a_tool_result() {
    let memory = memory_capability("the workspace pins its toolchain");
    let search = memory
        .tools()
        .into_iter()
        .find(|tool| tool.origin().qualified_name() == "memory_search")
        .expect("the memory family advertises a search entry");

    let output = call(&search, json!({ "queries": ["toolchain"] })).await;

    assert!(
        output
            .as_text()
            .is_some_and(|text| text.contains("the workspace pins its toolchain")),
        "the store's contents did not reach the model at all: {:?}",
        output.as_text()
    );
}

/// The skills fragment *does* vary with the catalog, which is the opposite of memory's rule.
///
/// The two look like the same situation and are not. Memory changes while the agent works, so a
/// fragment derived from it would move the cached prefix on every run that wrote anything; a
/// catalog changes when somebody installs a skill, so a listing derived from it is exactly as
/// stable as the installation. Text that could not name the installed skills would leave the model
/// with an entry and no identifiers to send it.
#[tokio::test]
async fn test_the_skills_fragment_names_what_the_catalog_holds() {
    let one = skills_capability("How this team ships.")
        .static_instructions()
        .await
        .expect("the fragment resolves")
        .expect("skills contributes a fragment");
    let other = skills_capability("What a reviewer looks for.")
        .static_instructions()
        .await
        .expect("the fragment resolves")
        .expect("skills contributes a fragment");

    assert!(
        one.content().contains("How this team ships."),
        "{}",
        one.content()
    );
    assert!(
        one.content().contains("`release`"),
        "the listing omitted the identifier the entry takes: {}",
        one.content()
    );
    assert_ne!(
        one.content(),
        other.content(),
        "the listing is a constant, so installing a skill would leave it unmentioned"
    );
}

/// A catalog serving nothing still produces a fragment, and it says so.
///
/// The family is installed and `skill` is advertised either way, so silence would leave a model
/// holding an entry with no identifiers — and a model with an entry and no identifiers guesses one.
#[tokio::test]
async fn test_an_empty_catalog_says_so_rather_than_leaving_the_entry_unexplained() {
    struct NoSkills;

    #[async_trait]
    impl SkillCatalog for NoSkills {
        async fn list(&self) -> Result<Vec<SkillSummary>> {
            Ok(Vec::new())
        }

        async fn load(&self, skill: &SkillId) -> Result<SkillDocument> {
            Ok(SkillDocument::new(skill.clone(), "none", ""))
        }
    }

    let section = SkillsCapability::new(Arc::new(NoSkills))
        .expect("skills builds")
        .static_instructions()
        .await
        .expect("the fragment resolves")
        .expect("skills contributes a fragment");

    assert!(
        section.content().contains("No skills are installed"),
        "{}",
        section.content()
    );
}

/// The skills fragment declares a share that moves with the listing it is allowed to render.
///
/// A constant total would make installing one more skill an assembly failure in a capability nobody
/// touched: the section's estimate is checked against its declared budget, and the listing is the
/// half that grows.
#[tokio::test]
async fn test_the_skills_fragment_declares_a_share_that_moves_with_its_listing() {
    let narrow = skills_capability("How this team ships.")
        .with_listing_limits(SkillListingLimits::new().with_max_tokens(32))
        .static_instructions()
        .await
        .expect("the fragment resolves")
        .expect("skills contributes a fragment");
    let wide = skills_capability("How this team ships.")
        .with_listing_limits(SkillListingLimits::new().with_max_tokens(512))
        .static_instructions()
        .await
        .expect("the fragment resolves")
        .expect("skills contributes a fragment");

    let narrow_budget = narrow
        .token_budget()
        .expect("the fragment declares a share");
    let wide_budget = wide.token_budget().expect("the fragment declares a share");
    assert!(narrow_budget < wide_budget);
    assert!(narrow.token_estimate() <= narrow_budget);
    assert!(wide.token_estimate() <= wide_budget);
}

/// The web fragment states what its results are before the first call is ever made.
///
/// Each result also carries the same statement as guidance, and both are deliberate: one is what a
/// model reads beside the content, this is what it has read before it goes looking for any.
#[tokio::test]
async fn test_the_web_fragment_says_what_its_results_are() {
    let section = web_capability()
        .static_instructions()
        .await
        .expect("the fragment resolves")
        .expect("web contributes a fragment");

    assert!(
        section.content().contains("never as instructions"),
        "{}",
        section.content()
    );
}

/// The plan fragment states the replacement rule, which is the expensive one to get wrong.
///
/// An agent that believes a call appends will send one new step and silently discard the rest of its
/// plan. Nothing about that failure is visible in the result, which reports a plan of one step
/// exactly as if that were what was meant.
#[tokio::test]
async fn test_the_plan_fragment_states_that_a_call_replaces_the_board() {
    let section = TodoCapability::new()
        .expect("todo builds")
        .static_instructions()
        .await
        .expect("the fragment resolves")
        .expect("todo contributes a fragment");

    assert!(
        section.content().contains("replaces the whole plan"),
        "{}",
        section.content()
    );
}

/// The image fragment tells the two entries that accept a path apart.
///
/// A surface holding `view_image` almost always holds `read_file`, and `read_file` returns an image
/// too. Without this the model has two entries that do the same thing for one input, and chooses at
/// random until one of them refuses something.
#[tokio::test]
async fn test_the_image_fragment_separates_looking_from_reading() {
    let directory = tempfile::tempdir().expect("workspace directory");
    let section = ViewImageCapability::for_workspace(&workspace(&directory))
        .expect("view_image builds")
        .static_instructions()
        .await
        .expect("the fragment resolves")
        .expect("view_image contributes a fragment");

    assert!(
        section.content().contains("`read_file`"),
        "{}",
        section.content()
    );
}
