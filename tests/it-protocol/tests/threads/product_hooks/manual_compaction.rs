// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.

use super::*;

fn summary() -> ModelResponse {
    final_message(
        &json!({
            "primary_request_and_intent": "resume working",
            "key_technical_concepts": "concepts",
            "files_and_code_sections": "files",
            "errors_and_fixes": "errors",
            "problem_solving": "solutions",
            "pending_tasks": "tasks",
            "current_work": "compact summary sentinel",
            "optional_next_step": "continue"
        })
        .to_string(),
    )
}

fn matched(event: &str, matcher: &str, command: &str) -> String {
    format!(
        "[[hooks.{event}]]\nmatcher = '{matcher}'\n[[hooks.{event}.hooks]]\ntype = 'command'\ncommand = '''{command}'''\n"
    )
}

async fn compact(server: &Server, connection: &mut Connection, thread: &str) {
    let reply = server
        .call(
            connection,
            "thread/compact/start",
            json!({"threadId": thread}),
        )
        .await;
    assert_eq!(reply, json!({}));
}

fn item_event<'a>(messages: &'a [Value], method: &str) -> Option<(usize, &'a Value)> {
    messages.iter().enumerate().find(|(_, event)| {
        event["method"] == method && event["params"]["item"]["type"] == "contextCompaction"
    })
}

#[tokio::test]
async fn manual_compaction_replaces_short_history_and_runs_matched_hooks_in_order() {
    let rollout_dir = tempfile::tempdir().unwrap();
    let mut home = HooksHome::new();
    home.store = Arc::new(RolloutThreadDirectory::new(rollout_dir.path()));
    let session_start = home.path("session-start.jsonl");
    let (pre, auto, post) = (
        home.path("pre.jsonl"),
        home.path("auto.jsonl"),
        home.path("post.jsonl"),
    );
    let block = format!(
        "{}; printf '%s' '{{\"decision\":\"block\",\"reason\":\"unsupported\"}}'",
        logging(&pre)
    );
    home.write_hooks(
        &[
            matched("PreCompact", "manual", &block),
            matched("PreCompact", "auto", &logging(&auto)),
            matched("PostCompact", "manual", &logging(&post)),
            hook_section("SessionStart", &logging(&session_start)),
        ]
        .join("\n"),
    );
    let server = home.server();
    let mut connection = server.connect(1).await;
    let thread = start_thread(&server, &mut connection).await;
    home.script.answer("obsolete assistant answer");
    server
        .run_turn(&mut connection, &thread, "original user request")
        .await;
    connection.passed.clear();

    let gate = Arc::new(Notify::new());
    home.script.push(Step::After(Arc::clone(&gate), summary()));
    compact(&server, &mut connection, &thread).await;
    let started = connection.notification("item/started").await;
    assert_eq!(started["item"]["type"], "contextCompaction");
    assert_eq!(started["threadId"], thread);
    home.script.wait_for_calls(2).await;
    assert!(logged(&post).is_empty());
    assert!(home.script.last_request().tools().is_empty());
    gate.notify_one();
    let ended = connection.notification("turn/completed").await;
    assert_eq!(ended["turn"]["status"], "completed");
    assert_eq!(ended["turn"]["id"], started["turnId"]);
    assert_eq!(home.script.requests.lock().unwrap().len(), 2);
    let events: Vec<_> = connection.passed.drain(..).collect();
    assert!(
        hook_notifications(&events)
            .iter()
            .any(|(method, event, status)| {
                method == "hook/completed" && event == "preCompact" && status == "failed"
            })
    );
    let (completed_at, completed) = item_event(&events, "item/completed").unwrap();
    assert_eq!(completed["params"]["item"]["id"], started["item"]["id"]);
    let post_at = events
        .iter()
        .position(|event| {
            event["method"] == "hook/started"
                && event["params"]["run"]["eventName"] == "postCompact"
        })
        .unwrap();
    assert!(completed_at < post_at);

    assert!(!auto.exists());
    for (path, name) in [(&pre, "PreCompact"), (&post, "PostCompact")] {
        let inputs = logged(path);
        assert_eq!(inputs.len(), 1);
        assert_eq!(inputs[0]["trigger"], "manual");
        assert_eq!(inputs[0]["hook_event_name"], name);
        for field in [
            "reason",
            "phase",
            "implementation",
            "compact_summary",
            "status",
            "error",
        ] {
            assert!(inputs[0].get(field).is_none(), "{field}");
        }
    }

    assert_eq!(logged(&session_start).len(), 1);
    home.script.answer("continued");
    server
        .run_turn(&mut connection, &thread, "continue live")
        .await;
    assert_compacted_request(&home);
    let starts = logged(&session_start);
    assert_eq!(starts.len(), 2);
    assert_eq!(starts[1]["source"], "compact");
    super::super::unload(&server, &mut connection, &thread).await;
    let resumed = server
        .call(
            &mut connection,
            "thread/resume",
            json!({"threadId": thread}),
        )
        .await;
    assert!(
        resumed["thread"]["turns"]
            .to_string()
            .contains("contextCompaction")
    );
    home.script.answer("resumed");
    server
        .run_turn(&mut connection, &thread, "continue after resume")
        .await;
    assert_compacted_request(&home);
}

fn assert_compacted_request(home: &HooksHome) {
    let input = serde_json::to_string(home.script.last_request().input()).unwrap();
    assert!(input.contains("original user request"), "{input}");
    assert!(input.contains("compact summary sentinel"), "{input}");
    assert!(!input.contains("obsolete assistant answer"), "{input}");
}

#[tokio::test]
async fn manual_pre_compact_stop_leaves_history_without_a_summary_request() {
    check_stop("PreCompact", false).await;
}

#[tokio::test]
async fn manual_post_compact_stop_keeps_the_replacement_after_resume() {
    check_stop("PostCompact", true).await;
}

async fn check_stop(event: &str, committed: bool) {
    let home = HooksHome::new();
    home.write_hooks(&matched(
        event,
        "manual",
        r#"printf '%s' '{"continue":false,"stopReason":"stop compaction"}'"#,
    ));
    let server = home.server();
    let mut connection = server.connect(1).await;
    let thread = start_thread(&server, &mut connection).await;
    home.script.answer("obsolete assistant answer");
    server
        .run_turn(&mut connection, &thread, "original user request")
        .await;
    connection.passed.clear();
    if committed {
        home.script.push(Step::Respond(summary()));
    }
    compact(&server, &mut connection, &thread).await;
    let ended = connection.notification("turn/completed").await;
    assert_eq!(ended["turn"]["status"], "interrupted");
    assert_eq!(
        home.script.requests.lock().unwrap().len(),
        1 + usize::from(committed)
    );
    let events: Vec<_> = connection.passed.drain(..).collect();
    assert_eq!(item_event(&events, "item/started").is_some(), committed);
    assert_eq!(item_event(&events, "item/completed").is_some(), committed);

    super::super::unload(&server, &mut connection, &thread).await;
    server
        .call(
            &mut connection,
            "thread/resume",
            json!({"threadId": thread}),
        )
        .await;
    home.script.answer("resumed");
    server
        .run_turn(&mut connection, &thread, "continue after resume")
        .await;
    if committed {
        assert_compacted_request(&home);
    } else {
        let input = serde_json::to_string(home.script.last_request().input()).unwrap();
        assert!(input.contains("obsolete assistant answer"), "{input}");
        assert!(!input.contains("compact summary sentinel"), "{input}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manual_compaction_acknowledges_before_the_replaced_turn_settles() {
    let home = HooksHome::new();
    let interrupts = home.path("interrupts.jsonl");
    let pre = home.path("pre.jsonl");
    home.write_hooks(
        &[
            hook_section("Interrupt", &logging(&interrupts)),
            matched("PreCompact", "manual", &logging(&pre)),
        ]
        .join("\n"),
    );
    let server = home.server();
    let mut connection = server.connect(1).await;
    let thread = start_thread(&server, &mut connection).await;
    let (release, gate) = std::sync::mpsc::channel();
    home.script
        .push(Step::AwaitCancellation(CancellationGate(gate)));
    let old = server
        .turn_start(&mut connection, &thread, "original user request")
        .await;
    home.script.wait_for_calls(1).await;
    home.script.push(Step::Respond(summary()));
    compact(&server, &mut connection, &thread).await;
    let rejected = server.call_error(
        &mut connection,
        "turn/start",
        json!({"threadId": thread, "input": [{"type": "text", "text": "rejected while settling"}]}),
    ).await;
    assert_eq!(rejected, compact_start_error());
    server
        .call(
            &mut connection,
            "thread/settings/update",
            json!({
                "threadId": thread, "model": "later-model",
            }),
        )
        .await;
    assert_eq!(home.script.requests.lock().unwrap().len(), 1);
    release.send(()).unwrap();
    let aborted = connection.notification("turn/completed").await;
    assert_eq!(aborted["turn"]["id"], old);
    assert_eq!(aborted["turn"]["status"], "interrupted");
    let completed = connection.notification("turn/completed").await;
    assert_ne!(completed["turn"]["id"], old);
    assert_eq!(completed["turn"]["status"], "completed");
    assert!(logged(&interrupts).is_empty());
    assert_eq!(logged(&pre)[0]["model"], "mock-model");
    let input = serde_json::to_string(home.script.last_request().input()).unwrap();
    assert!(input.contains("original user request"), "{input}");
    assert!(!input.contains("turn_aborted"), "{input}");
}

#[tokio::test]
async fn manual_compaction_reports_a_summary_failure_without_installing_history() {
    let home = HooksHome::new();
    let post = home.path("post.jsonl");
    home.write_hooks(&matched("PostCompact", "manual", &logging(&post)));
    let server = home.server();
    let mut connection = server.connect(1).await;
    let thread = start_thread(&server, &mut connection).await;
    home.script.answer("obsolete assistant answer");
    server
        .run_turn(&mut connection, &thread, "original user request")
        .await;
    connection.passed.clear();

    home.script.push(Step::Fail);
    compact(&server, &mut connection, &thread).await;
    let ended = connection.notification("turn/completed").await;
    assert_eq!(ended["turn"]["status"], "failed");
    let events: Vec<_> = connection.passed.drain(..).collect();
    assert!(item_event(&events, "item/started").is_some());
    assert!(item_event(&events, "item/completed").is_none());
    assert!(logged(&post).is_empty());
    home.script.answer("continued");
    server.run_turn(&mut connection, &thread, "continue").await;
    let input = serde_json::to_string(home.script.last_request().input()).unwrap();
    assert!(input.contains("obsolete assistant answer"), "{input}");
}

fn compact_start_error() -> Value {
    json!({
        "code": -32603,
        "message": "failed to submit turn input: ActiveTurnNotSteerable { turn_kind: Compact }"
    })
}

/// Ports the compact case of Codex's rejects_non_regular_turns through both input RPCs.
#[tokio::test]
async fn manual_compaction_is_stable_and_rejects_both_input_methods() {
    let home = HooksHome::new();
    home.write_hooks("");
    let server = home.server();
    let mut connection = server.connect_with(1, InitializeCapabilities::new()).await;
    let thread = start_thread(&server, &mut connection).await;
    let gate = Arc::new(Notify::new());
    home.script.push(Step::After(Arc::clone(&gate), summary()));
    compact(&server, &mut connection, &thread).await;
    let started = connection.notification("turn/started").await;
    let turn = &started["turn"]["id"];
    home.script.wait_for_calls(1).await;

    let input = json!([{"type": "text", "text": "rejected during compaction"}]);
    for (method, params, expected) in [
        (
            "turn/start",
            json!({"threadId": thread, "input": input}),
            compact_start_error(),
        ),
        (
            "turn/steer",
            json!({"threadId": thread, "input": input, "expectedTurnId": turn}),
            json!({
                "code": -32600,
                "message": "cannot steer a compact turn",
                "data": {
                    "message": "cannot steer a compact turn",
                    "codexErrorInfo": {"activeTurnNotSteerable": {"turnKind": "compact"}},
                    "additionalDetails": null,
                    "misalignment": null,
                }
            }),
        ),
    ] {
        let error = server.call_error(&mut connection, method, params).await;
        assert_eq!(error, expected, "{method}");
    }
    gate.notify_one();
    let completed = connection.notification("turn/completed").await;
    assert_eq!(&completed["turn"]["id"], turn);
    assert_eq!(completed["turn"]["status"], "completed");

    home.script.answer("done");
    server
        .run_turn(&mut connection, &thread, "accepted after compaction")
        .await;
    assert_eq!(home.script.requests.lock().unwrap().len(), 2);
    let input = serde_json::to_string(home.script.last_request().input()).unwrap();
    assert!(!input.contains("rejected during compaction"), "{input}");
}
