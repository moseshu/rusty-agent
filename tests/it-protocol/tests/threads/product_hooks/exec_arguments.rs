// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! `exec_command` arguments through the default coding factory, as Codex declares and reads them:
//! the model is shown a closed object that requires only `cmd`, yet `ExecCommandArgs` ignores
//! arguments it does not read, `tty` and `yield_time_ms` refuse an explicit `null`, and `additional_permissions` is refused by the policy, with Codex's
//! words, while `exec_permission_approvals` is off.
use super::*;

fn call(arguments: Value) -> RunItem {
    RunItem::new(
        ItemId::new("exec"),
        RunItemKind::ToolCall(ToolCall::new(
            CallId::new("exec"),
            "exec_command",
            arguments,
        )),
    )
}

/// The advertised `exec_command` schema in the last request the model received.
fn advertised(script: &Script) -> Value {
    script
        .last_request()
        .tools()
        .iter()
        .find(|tool| tool.qualified_name() == "exec_command")
        .expect("exec_command is advertised")
        .input_schema()
        .clone()
}

fn model_text(script: &Script) -> String {
    script
        .last_request()
        .input()
        .iter()
        .find_map(|item| match item {
            ModelInputItem::ToolCallOutput(output) if output.call_id().as_str() == "exec" => {
                Some(output.output().to_string())
            }
            _ => None,
        })
        .expect("the model received the call's output")
}

/// Runs one `exec_command` call under `config` and returns what the model read.
async fn run_exec(config: &str, arguments: Value) -> String {
    run_exec_with_approval("never", config, arguments).await.0
}

/// What the model read, and the `exec_command` schema it was shown.
async fn run_exec_with_approval(approval: &str, config: &str, arguments: Value) -> (String, Value) {
    let home = HooksHome::new();
    home.write_hooks_with_approval(approval, config);
    let server = home.server();
    let mut connection = server.connect(1).await;
    let thread = start_thread(&server, &mut connection).await;
    home.script
        .push(Step::Respond(ModelResponse::new(vec![call(arguments)])));
    home.script.answer("completed");
    server.run_turn(&mut connection, &thread, "run").await;
    (model_text(&home.script), advertised(&home.script))
}

const PERMISSIONS_ON: &str = "[features]\nexec_permission_approvals = true\n";

/// The default product's command form and its `exec_permission_approvals` form both show the
/// model a closed object, and both run a call that sends only `cmd` or one that sends arguments
/// they do not read.
#[tokio::test]
async fn only_cmd_runs_in_the_default_product_with_and_without_additional_permissions() {
    for config in ["", PERMISSIONS_ON] {
        let text = run_exec(config, json!({"cmd": "printf ran"})).await;
        assert!(text.contains("ran"), "{config:?}: {text}");
        let (text, schema) = run_exec_with_approval(
            "never",
            config,
            json!({"cmd": "printf extra", "max_output_tokens": 5, "unrelated": true}),
        )
        .await;
        assert!(text.contains("extra"), "{config:?}: {text}");
        assert_eq!(schema["additionalProperties"], json!(false), "{config:?}");
        assert_eq!(schema["required"], json!(["cmd"]), "{config:?}");
    }
}

/// What Codex cannot decode stays refused before anything runs.
#[tokio::test]
async fn undecodable_arguments_are_refused_in_the_default_product() {
    for config in ["", PERMISSIONS_ON] {
        for arguments in [
            json!({"shell": "/bin/sh"}),
            json!({"cmd": "touch ran.txt", "tty": null}),
            json!({"cmd": "touch ran.txt", "yield_time_ms": null}),
            json!({"cmd": "touch ran.txt", "login": "yes"}),
        ] {
            let text = run_exec(config, arguments.clone()).await;
            assert!(
                text.contains("Invalid arguments"),
                "{config:?} {arguments}: {text}"
            );
        }
    }
}

/// With the feature off, `additional_permissions` decodes as Codex's arguments do, and the policy
/// refuses it with Codex's words instead of running the command. Codex reads the permissions as a
/// sandbox override (`apply_granted_turn_permissions`), so under `never` its approval-policy
/// refusal comes first; under `on-request` the disabled feature is named.
#[tokio::test]
async fn additional_permissions_are_refused_with_codex_words_while_the_feature_is_off() {
    let arguments = json!({
        "cmd": "printf ran",
        "additional_permissions": {"network": {"enabled": true}}
    });
    for (approval, message) in [
        (
            "never",
            "approval policy is Never; reject command — you cannot ask for escalated permissions if the approval policy is Never",
        ),
        (
            "on-request",
            "additional permissions are disabled; enable `features.exec_permission_approvals` before using `with_additional_permissions`",
        ),
    ] {
        let (text, _) = run_exec_with_approval(approval, "", arguments.clone()).await;
        assert!(text.contains(message), "{approval}: {text}");
        assert!(!text.contains("\"ran\""), "{approval}: {text}");
    }
}
