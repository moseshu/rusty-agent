//! Typed output belongs to the public receiving agent; a run override follows the transfer
//! unvalidated, as Codex forwards a turn's schema.

use super::*;
use ra_core::output::OutputSchema;

#[derive(Debug, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
struct ReviewOutput {
    summary: String,
}

#[tokio::test]
async fn structured_handoff_validates_the_receivers_contract_but_not_run_overrides() {
    for streamed in [false, true] {
        for override_schema in [false, true] {
            let receiver = reviewer(Vec::new())
                .to_builder()
                .output_type::<ReviewOutput>()
                .unwrap()
                .build()
                .unwrap();
            let source = planner(
                HandoffSpec::new(AgentId::new("reviewer"), handoff_schema()),
                Vec::new(),
            );
            // Under the override the receiver's declaration is replaced and nothing is validated.
            let final_text = if override_schema {
                r#"{"response":["override"]}"#
            } else {
                r#"{"summary":"reviewed"}"#
            };
            let model = ScriptedModel::new(vec![
                ModelResponse::new(vec![
                    commentary("source", "not JSON"),
                    transfer_call("transfer", "call-1"),
                ]),
                ModelResponse::new(vec![message("answer", final_text)]),
            ]);
            let mut request = run_request(source, receiver, &model, RunConfig::new());
            if override_schema {
                request =
                    request.with_output_schema(OutputSchema::for_type::<Vec<String>>().unwrap());
            }
            let result = drive(request, streamed).await.unwrap();
            assert_eq!(result.last_agent().id().as_str(), "reviewer");
            if override_schema {
                assert_eq!(result.final_output().unwrap().value(), &json!(["override"]));
                assert!(result.final_output_as::<ReviewOutput>().is_err());
            } else {
                assert_eq!(
                    result.final_output_as::<ReviewOutput>().unwrap().summary,
                    "reviewed"
                );
            }
        }
        let receiver = reviewer(Vec::new())
            .to_builder()
            .output_type::<ReviewOutput>()
            .unwrap()
            .build()
            .unwrap();
        let source = planner(
            HandoffSpec::new(AgentId::new("reviewer"), handoff_schema()),
            Vec::new(),
        );
        let model = ScriptedModel::new(vec![
            ModelResponse::new(vec![transfer_call("transfer", "call-1")]),
            ModelResponse::new(vec![message("bad", "not JSON")]),
        ]);
        let error = drive(
            run_request(source, receiver, &model, RunConfig::new()),
            streamed,
        )
        .await
        .unwrap_err();
        assert_eq!(error.code(), "output.invalid_final_output");
        assert_eq!(
            error.run_state().unwrap().current_agent().unwrap().as_str(),
            "reviewer"
        );
    }
}
