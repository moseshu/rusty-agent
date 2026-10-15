//! Input and output tripwires stop a run through the existing guardrail contract.

#[path = "../../support.rs"]
mod support;

use std::sync::Arc;

use async_trait::async_trait;
use rusty_agent::{
    guardrail::{GuardrailFinalOutput, GuardrailFunctionOutput, InputGuardrail, OutputGuardrail},
    prelude::*,
};
use support::{answer, require};

struct BlockInput;
#[async_trait]
impl InputGuardrail for BlockInput {
    fn name(&self) -> &'static str {
        "input_marker"
    }
    // Explicit tutorial policy: block before calling the model. The framework default is true.
    fn run_in_parallel(&self) -> bool {
        false
    }
    async fn check(
        &self,
        _: &RunContext,
        input: &[ModelInputItem],
    ) -> Result<GuardrailFunctionOutput> {
        let blocked = input.iter().any(|item| match item {
            ModelInputItem::Message(message) => message.text_content().contains("BLOCK_INPUT"),
            _ => false,
        });
        Ok(if blocked {
            GuardrailFunctionOutput::tripwire()
        } else {
            GuardrailFunctionOutput::pass()
        })
    }
}

struct BlockOutput;
#[async_trait]
impl OutputGuardrail for BlockOutput {
    fn name(&self) -> &'static str {
        "output_marker"
    }
    async fn check(
        &self,
        _: &RunContext,
        output: &GuardrailFinalOutput<'_>,
    ) -> Result<GuardrailFunctionOutput> {
        Ok(if output.text().contains("BLOCK_OUTPUT") {
            GuardrailFunctionOutput::tripwire()
        } else {
            GuardrailFunctionOutput::pass()
        })
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let example = support::setup(vec![
        Box::new(|_| Ok(answer("Hello, Rust!"))),
        Box::new(|_| Ok(answer("BLOCK_OUTPUT"))),
    ])
    .await?;
    // tutorial:guardrail:start
    let agent = AgentSpec::builder()
        .id(AgentId::new("checked"))
        .name("Checked assistant")
        .instructions("Repeat the requested greeting or marker exactly.")
        .input_guardrail(Arc::new(BlockInput))
        .output_guardrail(Arc::new(BlockOutput))
        .build()?;
    let result = example.runner.run(agent.clone(), "Hello, Rust!").await?;
    println!("{}", result.final_text());
    // tutorial:guardrail:end
    for (input, expected_code) in [
        ("BLOCK_INPUT", "guardrail.input"),
        ("BLOCK_OUTPUT", "guardrail.output"),
    ] {
        match example.runner.run(agent.clone(), input).await {
            Err(error) if error.code() == expected_code => {
                println!("Expected tripwire: {}", error.code());
            }
            Err(error) => return Err(error),
            Ok(_) => {
                return Err(Error::caller(
                    "expected guardrail tripwire was not triggered",
                ));
            }
        }
    }
    if example.check {
        require(
            result.state().input_guardrail_results().len() == 1,
            "missing input verdict",
        )?;
        require(
            result.state().output_guardrail_results().len() == 1,
            "missing output verdict",
        )?;
    }
    example.finish().await
}
