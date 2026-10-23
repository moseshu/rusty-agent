//! Pages must pass the host projector unchanged, including escape-heavy UTF-8 data.
use super::*;
use ra_context::budget::ToolResultBudget;
use ra_core::tool::ToolConcurrency;

#[tokio::test]
async fn every_page_fits_the_byte_and_token_projector_without_a_second_truncation() {
    for contents in ["ascii \"\\\n".repeat(20000), "技界🦀\n".repeat(20000)] {
        for budget in [
            ToolResultBudget::default(),
            ToolResultBudget::new(2000, 500).unwrap(),
        ] {
            let fixture = super::read::ExecutorFixture::new(&contents);
            let pair = fixture.pair(
                budget
                    .max_bytes()
                    .min(budget.max_tokens() * ra_core::prompt::CHARS_PER_TOKEN),
            );
            let mut args = fixture.reference_args();
            let mut joined = String::new();
            loop {
                let output = call(pair[1].as_ref(), args.clone()).await.unwrap();
                let before = output.model_blocks();
                let projected = budget
                    .project_output(&RunId::new("projection"), &CallId::new("page"), output)
                    .unwrap();
                assert_eq!(projected.model_blocks(), before);
                assert!(projected.metadata().truncations().is_empty());
                let page: Value = serde_json::from_str(projected.as_text().unwrap()).unwrap();
                joined.push_str(page["contents"].as_str().unwrap());
                if page["next_cursor"].is_null() {
                    break;
                }
                args["cursor"] = page["next_cursor"].clone();
            }
            assert_eq!(joined, contents);
        }
    }
}

#[tokio::test]
async fn all_callers_use_the_projector_budget_and_preserve_json_and_cursors_through_dispatch() {
    use ra_core::{cancel::CancelScope, tool::ToolServices};
    use ra_runtime::tool::dispatch::{
        CallHistory, ToolDispatch, ToolDispatchRequest, dispatch_tool,
    };
    let budget = ToolResultBudget::new(1000, 250).unwrap();
    let contents = "x".repeat(6000);
    let pair = tools(
        cloud(
            SkillCatalog {
                entries: vec![entry(
                    SkillSourceKind::Cloud,
                    CLOUD_AUTHORITY_ID,
                    "pkg",
                    "skill://pkg/SKILL.md",
                )],
                warnings: vec![],
            },
            &contents,
        ),
        1000,
    );
    for tool in &pair {
        tool.validate().unwrap();
        assert_eq!(tool.options().concurrency(), ToolConcurrency::Parallel);
        assert!(!tool.schema().strict_json_schema());
    }
    for caller in [ToolCaller::Direct, ToolCaller::Programmatic] {
        let mut args = json!({"package":"pkg"});
        let mut joined = String::new();
        let mut count = 0;
        loop {
            let request = ToolDispatchRequest::new(
                pair[1].clone(),
                CallId::new(format!("page-{count}")),
                args.clone(),
                Arc::new(run()),
                CancelScope::root(),
                CallHistory::default(),
                Default::default(),
            )
            .with_caller(caller)
            .with_services(ToolServices::new().with_output_projector(Arc::new(budget)));
            let result = dispatch_tool(request).await.unwrap();
            let ToolDispatch::Observed(observation) = result.into_parts().0 else {
                panic!("expected a completed observation")
            };
            let output: ToolOutput =
                serde_json::from_value(observation.output().output().clone()).unwrap();
            assert!(output.model_excerpt().is_none());
            assert!(output.metadata().truncations().is_empty());
            assert!(output.as_text().unwrap().len() <= 1000);
            let page: Value = serde_json::from_str(output.as_text().unwrap()).unwrap();
            joined.push_str(page["contents"].as_str().unwrap());
            count += 1;
            if page["next_cursor"].is_null() {
                break;
            }
            args["cursor"] = page["next_cursor"].clone();
        }
        assert!(count > 1);
        assert_eq!(joined, contents);
    }
}
