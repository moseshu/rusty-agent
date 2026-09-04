//! The built-in capabilities: what each one contributes, and what it deliberately does not.

use std::sync::Arc;

use ra_core::{
    agent::AgentSpec,
    capability::{Capability, CapabilityFamily},
    context::RunContext,
    item::{AgentId, CallId},
    model::ModelSettings,
    permission::PermissionScope,
    state::RunId,
    tool::{Tool, ToolContext, ToolOutput},
};
use ra_exec::{fs::Workspace, session::ProcessManager};
use ra_tools::capability::{
    ApplyPatchCapability, FilesystemCapability, SearchCapability, ShellCapability,
};
use serde_json::json;
use tempfile::TempDir;

fn workspace(directory: &TempDir) -> Workspace {
    Workspace::open(directory.path()).expect("workspace opens")
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
    let apply_patch = ApplyPatchCapability::for_workspace(&workspace).expect("apply_patch builds");
    let shell = ShellCapability::for_workspace(&workspace, Arc::new(ProcessManager::default()))
        .expect("shell builds");

    assert_eq!(filesystem.kind(), CapabilityFamily::FILESYSTEM);
    assert_eq!(advertised_names(&filesystem), ["read_file"]);
    assert_eq!(search.kind(), CapabilityFamily::SEARCH);
    assert_eq!(advertised_names(&search), ["grep", "glob"]);
    assert_eq!(apply_patch.kind(), CapabilityFamily::APPLY_PATCH);
    assert_eq!(advertised_names(&apply_patch), ["apply_patch"]);
    assert_eq!(shell.kind(), CapabilityFamily::SHELL);
    assert_eq!(advertised_names(&shell), ["exec_command", "write_stdin"]);
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

    let changing: Vec<Box<dyn Capability>> = vec![
        Box::new(ApplyPatchCapability::for_workspace(&workspace).expect("apply_patch builds")),
        Box::new(
            ShellCapability::for_workspace(&workspace, Arc::new(ProcessManager::default()))
                .expect("shell builds"),
        ),
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

/// None of the built-in four requires another, so a host may install any subset.
///
/// A dependency is a hard assembly error and an ordering edge, not documentation of a habit.
/// Declaring `apply_patch` -> `filesystem` because editing usually follows reading would refuse a
/// configuration that reads through the shell, which is a configuration that works.
#[test]
fn test_the_built_in_capabilities_require_nothing_of_each_other() {
    let directory = tempfile::tempdir().expect("workspace directory");
    let workspace = workspace(&directory);

    let capabilities: Vec<Box<dyn Capability>> = vec![
        Box::new(FilesystemCapability::for_workspace(&workspace).expect("filesystem builds")),
        Box::new(SearchCapability::for_workspace(&workspace).expect("search builds")),
        Box::new(ApplyPatchCapability::for_workspace(&workspace).expect("apply_patch builds")),
        Box::new(
            ShellCapability::for_workspace(&workspace, Arc::new(ProcessManager::default()))
                .expect("shell builds"),
        ),
    ];
    for capability in &capabilities {
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
    let workspace = workspace(&directory);

    let capabilities: Vec<Box<dyn Capability>> = vec![
        Box::new(FilesystemCapability::for_workspace(&workspace).expect("filesystem builds")),
        Box::new(SearchCapability::for_workspace(&workspace).expect("search builds")),
        Box::new(ApplyPatchCapability::for_workspace(&workspace).expect("apply_patch builds")),
        Box::new(
            ShellCapability::for_workspace(&workspace, Arc::new(ProcessManager::default()))
                .expect("shell builds"),
        ),
    ];
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
    let workspace = workspace(&directory);

    let capabilities: Vec<Box<dyn Capability>> = vec![
        Box::new(FilesystemCapability::for_workspace(&workspace).expect("filesystem builds")),
        Box::new(SearchCapability::for_workspace(&workspace).expect("search builds")),
        Box::new(ApplyPatchCapability::for_workspace(&workspace).expect("apply_patch builds")),
        Box::new(
            ShellCapability::for_workspace(&workspace, Arc::new(ProcessManager::default()))
                .expect("shell builds"),
        ),
    ];

    for capability in &capabilities {
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
