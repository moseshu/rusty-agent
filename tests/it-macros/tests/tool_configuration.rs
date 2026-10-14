//! Function-tool configuration is validated without changing application name resolution.

use std::{
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

use core_contract::{
    agent::{AgentId, AgentSpec},
    context::RunContext,
    error::{Error, Result},
    item::CallId,
    state::RunId,
    tool::{Tool, ToolCaller, ToolConcurrency, ToolContext, ToolOptions, ToolOutput},
};
use ra_macros::tool;
use serde_json::json;

mod application_defaults {
    fn options() -> String {
        "options".to_owned()
    }
    fn tool() -> String {
        "tool".to_owned()
    }
    fn tool_with_options() -> String {
        "replacement".to_owned()
    }

    #[ra_macros::tool]
    pub fn echo(
        #[tool(default = format!("{}/{}/{}", options(), tool(), tool_with_options()))]
        value: String,
    ) -> String {
        value
    }
}

mod application_policies {
    use super::*;

    fn options(_: &RunContext) -> Result<bool> {
        Ok(true)
    }
    fn tool(_: &ToolContext<'_>) -> Result<bool> {
        Ok(true)
    }
    fn tool_with_options(_: &ToolContext<'_>, _: &Error) -> Result<Option<ToolOutput>> {
        Ok(Some(ToolOutput::text("handled by the application")))
    }

    #[ra_macros::tool(
        is_enabled = sync(options),
        needs_approval = sync(tool),
        failure_error_function = sync(tool_with_options)
    )]
    pub async fn fail() -> Result<()> {
        Err(Error::caller("application failure"))
    }
}

mod application_adjustment {
    use super::*;

    fn options(options: ToolOptions) -> ToolOptions {
        options.with_concurrency(ToolConcurrency::Exclusive)
    }

    #[ra_macros::tool(options = options)]
    pub fn echo() -> &'static str {
        "adjusted"
    }
}

mod hook_parameter_names {
    use super::*;

    fn context(_: &ToolContext<'_>) -> Result<bool> {
        Ok(true)
    }
    fn error(_: &ToolContext<'_>, _: &Error) -> Result<Option<ToolOutput>> {
        Ok(Some(ToolOutput::text("handled by error()")))
    }

    #[ra_macros::tool(needs_approval = sync(context), failure_error_function = sync(error))]
    pub async fn fail() -> Result<()> {
        Err(Error::caller("application failure"))
    }
}

mod imported_ok {
    /// Stands in for `use anyhow::Ok;`: a glob-visible `Ok` must not replace the prelude's in
    /// generated code.
    mod library {
        #[allow(non_snake_case)]
        pub fn Ok<T>(value: T) -> std::result::Result<T, String> {
            std::result::Result::Ok(value)
        }
    }
    use library::Ok;

    #[ra_macros::tool(is_enabled = true, needs_approval = false)]
    pub async fn answer() -> i32 {
        Ok(42).unwrap()
    }
}

#[tokio::test]
async fn generated_hooks_and_results_do_not_shadow_application_names() {
    let tool = hook_parameter_names::fail::tool().unwrap();
    let agent = AgentSpec::builder()
        .id(AgentId::new("hooks"))
        .name("Hooks")
        .build()
        .unwrap();
    let run = RunContext::new(RunId::new("hooks"), &agent);
    let call = CallId::new("hook-call");
    let arguments = json!({});
    let context = ToolContext::new(&run, &tool, &call, &arguments);
    assert!(tool.needs_approval(&context).await.unwrap());
    let error = tool.call(context).await.unwrap_err();
    assert_eq!(
        tool.handle_failure(&ToolContext::new(&run, &tool, &call, &arguments), &error)
            .await
            .unwrap()
            .unwrap()
            .as_text(),
        Some("handled by error()")
    );

    let tool = imported_ok::answer::tool().unwrap();
    let context = ToolContext::new(&run, &tool, &call, &arguments);
    assert!(tool.is_enabled(&run).await.unwrap());
    assert!(!tool.needs_approval(&context).await.unwrap());
    assert_eq!(tool.call(context).await.unwrap().as_text(), Some("42"));
}

#[tokio::test]
async fn application_default_functions_can_share_constructor_names() {
    assert_eq!(application_defaults::echo("direct".to_owned()), "direct");
    let tool = application_defaults::echo::tool().unwrap();
    let agent = AgentSpec::builder()
        .id(AgentId::new("defaults"))
        .name("Defaults")
        .build()
        .unwrap();
    let run = RunContext::new(RunId::new("defaults"), &agent);
    let call = CallId::new("default-call");
    let arguments = json!({});
    let output = tool
        .call(ToolContext::new(&run, &tool, &call, &arguments))
        .await
        .unwrap();
    assert_eq!(output.as_text(), Some("options/tool/replacement"));
}

#[tokio::test]
async fn application_policy_functions_can_share_constructor_names() {
    let tool = application_policies::fail::tool().unwrap();
    let agent = AgentSpec::builder()
        .id(AgentId::new("policies"))
        .name("Policies")
        .build()
        .unwrap();
    let run = RunContext::new(RunId::new("policies"), &agent);
    let call = CallId::new("policy-call");
    let arguments = json!({});
    let context = ToolContext::new(&run, &tool, &call, &arguments);
    assert!(tool.is_enabled(&run).await.unwrap());
    assert!(tool.needs_approval(&context).await.unwrap());
    let error = tool.call(context).await.unwrap_err();
    assert_eq!(
        tool.handle_failure(&ToolContext::new(&run, &tool, &call, &arguments), &error)
            .await
            .unwrap()
            .unwrap()
            .as_text(),
        Some("handled by the application")
    );
    assert_eq!(
        application_adjustment::echo::options()
            .unwrap()
            .concurrency(),
        ToolConcurrency::Exclusive
    );
    assert!(application_adjustment::echo::tool().is_ok());
}

#[tool(timeout = Duration::ZERO)]
async fn zero_duration() {}

#[tool(options = |options| options.with_timeout(Duration::ZERO))]
async fn zero_adjustment() {}

#[tool]
async fn replacement() {}

#[test]
fn zero_timeouts_are_rejected_at_every_configuration_entry() {
    assert!(
        zero_duration::options()
            .unwrap_err()
            .to_string()
            .contains("greater than zero")
    );
    assert!(
        zero_duration::tool()
            .err()
            .unwrap()
            .to_string()
            .contains("greater than zero")
    );
    assert!(
        zero_adjustment::options()
            .unwrap_err()
            .to_string()
            .contains("greater than zero")
    );
    assert!(zero_adjustment::tool().is_err());
    assert!(
        replacement::tool_with_options(ToolOptions::new().with_timeout(Duration::ZERO))
            .err()
            .unwrap()
            .to_string()
            .contains("greater than zero")
    );
    assert!(
        zero_duration::tool_with_options(ToolOptions::new().with_timeout(Duration::from_nanos(1)))
            .is_ok()
    );
}

#[tool(timeout = 1e-9)]
async fn one_nanosecond() {}

#[tool(timeout = 1.25)]
async fn fractional_seconds() {}

#[test]
fn representable_positive_timeouts_keep_seconds_and_nanoseconds() {
    assert_eq!(
        one_nanosecond::tool().unwrap().options().timeout(),
        Some(Duration::from_nanos(1))
    );
    assert_eq!(
        fractional_seconds::tool().unwrap().options().timeout(),
        Some(Duration::new(1, 250_000_000))
    );
}

const DIRECT: ToolCaller = ToolCaller::Direct;

#[tool(allowed_callers = [DIRECT, DIRECT])]
async fn typed_duplicates() {}

#[tool(allowed_callers = ["direct", DIRECT])]
async fn mixed_duplicates() {}

static CALLER_EVALUATIONS: AtomicUsize = AtomicUsize::new(0);
fn evaluated_caller() -> ToolCaller {
    CALLER_EVALUATIONS.fetch_add(1, Ordering::SeqCst);
    ToolCaller::Direct
}

#[tool(allowed_callers = [evaluated_caller(), evaluated_caller()])]
async fn evaluated_duplicates() {}

#[tool(allowed_callers = [ToolCaller::Programmatic, "direct"])]
async fn distinct_callers() {}

#[test]
fn duplicate_callers_are_rejected_after_resolving_expressions() {
    for options in [
        typed_duplicates::options(),
        mixed_duplicates::options(),
        evaluated_duplicates::options(),
    ] {
        assert!(
            options
                .unwrap_err()
                .to_string()
                .contains("duplicate caller")
        );
    }
    assert_eq!(CALLER_EVALUATIONS.load(Ordering::SeqCst), 2);
    assert!(typed_duplicates::tool().is_err());
    assert!(mixed_duplicates::tool().is_err());
    assert!(evaluated_duplicates::tool().is_err());
    assert_eq!(CALLER_EVALUATIONS.load(Ordering::SeqCst), 4);
    assert_eq!(
        distinct_callers::tool()
            .unwrap()
            .options()
            .allowed_callers(),
        Some(&[ToolCaller::Direct, ToolCaller::Programmatic][..])
    );
    assert!(
        replacement::tool_with_options(ToolOptions::new().with_allowed_callers([DIRECT, DIRECT]))
            .is_ok()
    );
}
