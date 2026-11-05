// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Output-spill cases ported from Codex's hook_output tests through the public test adapter.
use ra_coding::hooks::test_api::{AdditionalContext, AdditionalContextLimit, HookOutputSpiller};
use ra_core::session::SessionId;

fn spiller() -> HookOutputSpiller {
    HookOutputSpiller::new(SessionId::generate())
}
fn spill_path(output: &str) -> &str {
    output
        .lines()
        .find_map(|line| line.strip_prefix("Full hook output saved to: "))
        .unwrap()
}
#[tokio::test]
async fn small_hook_output_remains_inline() {
    assert_eq!(spiller().maybe_spill_text("short".into()).await, "short");
}
#[tokio::test]
async fn large_hook_output_spills_to_file() {
    let text = "hook output ".repeat(1000);
    let output = spiller().maybe_spill_text(text.clone()).await;
    assert!(output.contains("tokens truncated"));
    let path = spill_path(&output);
    assert_eq!(std::fs::read_to_string(path).unwrap(), text);
    std::fs::remove_dir_all(std::path::Path::new(path).parent().unwrap()).unwrap();
}
#[tokio::test]
async fn additional_contexts_apply_limits_individually() {
    let limited = "limited hook output ".repeat(1000);
    let unlimited = "unlimited hook output ".repeat(5000);
    let outputs = spiller()
        .maybe_spill_additional_contexts(vec![
            AdditionalContext {
                text: limited.clone(),
                limit: AdditionalContextLimit::from_config(Some(1)),
            },
            AdditionalContext {
                text: unlimited.clone(),
                limit: AdditionalContextLimit::from_config(Some(0)),
            },
            AdditionalContext {
                text: unlimited.clone(),
                limit: AdditionalContextLimit::from_config(Some(usize::MAX)),
            },
        ])
        .await;
    let path = spill_path(&outputs[0]);
    assert_eq!(std::fs::read_to_string(path).unwrap(), limited);
    assert_eq!(outputs[1], unlimited);
    assert_eq!(outputs[2], unlimited);
    std::fs::remove_dir_all(std::path::Path::new(path).parent().unwrap()).unwrap();
}
#[tokio::test]
async fn hook_output_spiller_is_scoped_to_its_thread() {
    let text = "hook output ".repeat(1000);
    let first = spiller().maybe_spill_text(text.clone()).await;
    let second = spiller().maybe_spill_text(text).await;
    let first = std::path::Path::new(spill_path(&first)).parent().unwrap();
    let second = std::path::Path::new(spill_path(&second)).parent().unwrap();
    assert_ne!(first, second);
    std::fs::remove_dir_all(first).unwrap();
    std::fs::remove_dir_all(second).unwrap();
}
