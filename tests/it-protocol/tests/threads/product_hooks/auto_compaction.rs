// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.

//! Automatic compaction across a thread's turns, through the coding factory and the app server.
//!
//! Codex measures the session's whole history against the model's auto-compaction limit, before a
//! turn records its prompt and between the steps of a turn that needs a follow-up, and replaces all
//! of it with the user messages and a summary. The usage it measures is the provider's last
//! reported total plus an estimate of what was recorded after it; these cases report totals far
//! over the 90% limit of the configured model's window.

use super::*;

/// A model with a known context window, so the coding factory's compaction capability is active.
const MODEL: &str = "gpt-5.4";
/// Over nine tenths of any window the built-in table knows.
const OVER_LIMIT: u64 = 5_000_000;
const SUMMARY_SENTINEL: &str = "auto compact summary sentinel";
/// The context the compact `SessionStart` hook adds ahead of the next prompt.
const COMPACT_CONTEXT: &str = "compact context noted";

fn summary() -> ModelResponse {
    final_message(
        &json!({
            "primary_request_and_intent": "keep going",
            "key_technical_concepts": "concepts",
            "files_and_code_sections": "files",
            "errors_and_fixes": "errors",
            "problem_solving": "solutions",
            "pending_tasks": "tasks",
            "current_work": SUMMARY_SENTINEL,
            "optional_next_step": "continue"
        })
        .to_string(),
    )
}

fn reporting(response: ModelResponse, total_tokens: u64) -> ModelResponse {
    response.with_usage(ra_core::usage::Usage::from_request(
        ra_core::usage::RequestUsage::new(total_tokens - 100, 100),
    ))
}

fn answer(text: &str, total_tokens: u64) -> Step {
    Step::Respond(reporting(final_message(text), total_tokens))
}

fn echo_call(id: &str, total_tokens: u64) -> Step {
    Step::Respond(reporting(
        ModelResponse::new(vec![RunItem::new(
            ItemId::new(format!("command-{id}")),
            RunItemKind::ToolCall(ToolCall::new(
                CallId::new(format!("call-{id}")),
                "exec_command",
                json!({"cmd": format!("echo obsolete tool output {id}"), "justification": null,
                    "prefix_rule": null, "sandbox_permissions": null, "workdir": null,
                    "shell": "/bin/sh", "tty": false, "login": false, "yield_time_ms": 1000,
                    "timeout_ms": null}),
            )),
        )]),
        total_tokens,
    ))
}

fn matched(event: &str, matcher: &str, command: &str) -> String {
    format!(
        "[[hooks.{event}]]\nmatcher = '{matcher}'\n[[hooks.{event}.hooks]]\ntype = 'command'\ncommand = '''{command}'''\n"
    )
}

fn write_config(home: &HooksHome, hooks: &[String]) {
    home.write_hooks(&hooks.join("\n"));
}

/// Starts a thread whose turns run on [`MODEL`].
async fn start(server: &Server, connection: &mut Connection) -> String {
    let thread = start_thread(server, connection).await;
    server
        .call(
            connection,
            "thread/settings/update",
            json!({"threadId": thread, "model": MODEL}),
        )
        .await;
    thread
}

fn request_text(request: &ModelRequest) -> String {
    serde_json::to_string(request.input()).unwrap()
}

fn is_summary_request(request: &ModelRequest) -> bool {
    request_text(request).contains("Summarize the conversation context")
}

fn compaction_events<'a>(messages: &'a [Value], method: &str) -> Vec<(usize, &'a Value)> {
    messages
        .iter()
        .enumerate()
        .filter(|(_, event)| {
            event["method"] == method && event["params"]["item"]["type"] == "contextCompaction"
        })
        .collect()
}

/// Ports `auto_compact_runs_after_token_limit_hit`, `auto_compact_emits_context_compaction_items`
/// and `auto_compact_starts_after_turn_started`: a turn after the limit was reached compacts before
/// it samples, and its request carries every user message and the summary instead of the answers.
/// Only the `auto` matchers run, and the context it re-establishes starts the turn.
#[tokio::test]
async fn a_turn_after_the_limit_compacts_the_whole_history_before_sampling() {
    let rollout_dir = tempfile::tempdir().unwrap();
    let mut home = HooksHome::new();
    home.store = Arc::new(RolloutThreadDirectory::new(rollout_dir.path()));
    let (pre, manual, post, session_start) = (
        home.path("pre.jsonl"),
        home.path("manual.jsonl"),
        home.path("post.jsonl"),
        home.path("session-start.jsonl"),
    );
    write_config(
        &home,
        &[
            matched("PreCompact", "auto", &logging(&pre)),
            matched("PreCompact", "manual", &logging(&manual)),
            matched("PostCompact", "auto", &logging(&post)),
            hook_section("SessionStart", &logging(&session_start)),
            matched(
                "SessionStart",
                "compact",
                &format!(
                    "printf '%s' '{{\"hookSpecificOutput\":{{\"hookEventName\":\"SessionStart\",\"additionalContext\":\"{COMPACT_CONTEXT}\"}}}}'"
                ),
            ),
        ],
    );
    let server = home.server();
    let mut connection = server.connect(1).await;
    let thread = start(&server, &mut connection).await;
    home.script.push(answer("obsolete first answer", 70_000));
    server
        .run_turn(&mut connection, &thread, "first user message")
        .await;
    home.script
        .push(answer("obsolete second answer", OVER_LIMIT));
    server
        .run_turn(&mut connection, &thread, "second user message")
        .await;
    assert!(
        !pre.exists(),
        "a single over-limit answer waits for the next turn"
    );
    connection.passed.clear();

    home.script.push(Step::Respond(summary()));
    home.script.push(answer("final answer", 1_000));
    let third = server
        .run_turn(&mut connection, &thread, "post auto user message")
        .await;

    let requests = home.script.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 4);
    assert_eq!(
        requests
            .iter()
            .filter(|request| is_summary_request(request))
            .count(),
        1
    );
    assert!(is_summary_request(&requests[2]));
    let compacted = request_text(&requests[2]);
    assert!(compacted.contains("obsolete second answer"), "{compacted}");
    assert!(!compacted.contains("post auto user message"), "{compacted}");
    assert_eq!(
        requests[2].system_instructions(),
        requests[0].system_instructions()
    );
    assert_compacted(&requests[3]);
    assert!(request_text(&requests[3]).contains("post auto user message"));
    assert!(request_text(&requests[3]).contains(COMPACT_CONTEXT));

    let events: Vec<_> = connection.passed.drain(..).collect();
    let live_prompt = events
        .iter()
        .find(|event| {
            matches!(
                event["method"].as_str(),
                Some("item/started" | "item/completed")
            ) && event["params"]["item"]["type"] == "userMessage"
        })
        .unwrap();
    assert_eq!(
        live_prompt["params"]["item"]["id"],
        format!("{third}-input-0")
    );
    let started = compaction_events(&events, "item/started");
    let completed = compaction_events(&events, "item/completed");
    assert_eq!((started.len(), completed.len()), (1, 1));
    assert_eq!(
        started[0].1["params"]["item"]["id"],
        completed[0].1["params"]["item"]["id"]
    );
    let turn_started = events
        .iter()
        .position(|event| event["method"] == "turn/started")
        .unwrap();
    assert!(
        turn_started < started[0].0,
        "compaction started before turn started"
    );

    assert!(!manual.exists());
    for log in [&pre, &post] {
        let inputs = logged(log);
        assert_eq!(inputs.len(), 1);
        assert_eq!(inputs[0]["trigger"], "auto");
    }
    let starts = logged(&session_start);
    assert_eq!(starts.last().unwrap()["source"], "compact");

    super::super::unload(&server, &mut connection, &thread).await;
    let resumed = server
        .call(
            &mut connection,
            "thread/resume",
            json!({"threadId": thread}),
        )
        .await;
    let turns = resumed["thread"]["turns"].as_array().unwrap();
    assert!(
        turns[2].to_string().contains("contextCompaction"),
        "{turns:?}"
    );
    assert!(
        !turns[1].to_string().contains("contextCompaction"),
        "{turns:?}"
    );
    // Neither the turn's start recorded before compacting nor the context the compact `SessionStart`
    // added ahead of the prompt moves its identity: the prompt keeps the id the live thread gave it
    // when the turn was accepted.
    let prompt = turns[2]["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["type"] == "userMessage")
        .unwrap();
    assert_eq!(prompt["id"], format!("{third}-input-0"), "{turns:?}");
    home.script.answer("resumed answer");
    server
        .run_turn(&mut connection, &thread, "continue after resume")
        .await;
    assert_eq!(home.script.requests.lock().unwrap().len(), 5);
    assert_compacted(&home.script.last_request());
}

fn assert_compacted(request: &ModelRequest) {
    let input = request_text(request);
    for text in [
        "first user message",
        "second user message",
        SUMMARY_SENTINEL,
    ] {
        assert!(input.contains(text), "{text}: {input}");
    }
    for text in [
        "obsolete first answer",
        "obsolete second answer",
        "obsolete tool output",
    ] {
        assert!(!input.contains(text), "{text}: {input}");
    }
}

/// Ports `multiple_auto_compact_per_task_runs_after_token_limit_hit`: steps that need a follow-up
/// past the limit compact between them, as often as they reach it, and the turn goes on sampling
/// from the replacement. The live thread and the one rebuilt from its rollout continue from the
/// same history, without the records the replacement folded in.
#[tokio::test]
async fn steps_past_the_limit_compact_mid_turn_and_the_turn_continues() {
    let rollout_dir = tempfile::tempdir().unwrap();
    let mut home = HooksHome::new();
    home.store = Arc::new(RolloutThreadDirectory::new(rollout_dir.path()));
    let pre = home.path("pre.jsonl");
    write_config(&home, &[matched("PreCompact", "auto", &logging(&pre))]);
    let server = home.server();
    let mut connection = server.connect(1).await;
    let thread = start(&server, &mut connection).await;
    home.script.push(echo_call("one", OVER_LIMIT));
    home.script.push(Step::Respond(summary()));
    home.script.push(echo_call("two", OVER_LIMIT));
    home.script.push(Step::Respond(summary()));
    home.script.push(answer("final answer", 1_000));
    server
        .run_turn(&mut connection, &thread, "first user message")
        .await;

    let requests = home.script.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 5);
    let summaries: Vec<bool> = requests.iter().map(is_summary_request).collect();
    assert_eq!(summaries, [false, true, false, true, false]);
    assert!(request_text(&requests[1]).contains("obsolete tool output one"));
    for request in [&requests[2], &requests[4]] {
        let input = request_text(request);
        assert!(input.contains("first user message"), "{input}");
        assert!(input.contains(SUMMARY_SENTINEL), "{input}");
        assert!(!input.contains("obsolete tool output one"), "{input}");
    }
    assert!(!request_text(&requests[4]).contains("obsolete tool output two"));
    assert_eq!(logged(&pre).len(), 2);
    let events: Vec<_> = connection.passed.drain(..).collect();
    assert_eq!(compaction_events(&events, "item/completed").len(), 2);

    home.script.answer("live answer");
    server
        .run_turn(&mut connection, &thread, "second user message")
        .await;
    let live = request_text(&home.script.last_request());
    super::super::unload(&server, &mut connection, &thread).await;
    server
        .call(
            &mut connection,
            "thread/resume",
            json!({"threadId": thread}),
        )
        .await;
    home.script.answer("resumed answer");
    server
        .run_turn(&mut connection, &thread, "second user message")
        .await;
    let resumed = request_text(&home.script.last_request());
    for input in [&live, &resumed] {
        assert!(input.contains(SUMMARY_SENTINEL), "{input}");
        assert!(!input.contains("obsolete tool output"), "{input}");
        assert!(input.contains("final answer"), "{input}");
    }
    assert_eq!(
        resumed.matches("second user message").count(),
        live.matches("second user message").count() + 1,
        "the resumed thread holds the live turn and its own"
    );
}

/// A summary request that fails before a turn records its prompt ends the turn with the error and
/// without sampling, and keeps the prompt, as Codex's `run_pre_sampling_compact` failure does.
#[tokio::test]
async fn a_failed_pre_turn_compaction_fails_the_turn_and_keeps_its_prompt() {
    let home = HooksHome::new();
    let post = home.path("post.jsonl");
    write_config(&home, &[matched("PostCompact", "auto", &logging(&post))]);
    let server = home.server();
    let mut connection = server.connect(1).await;
    let thread = start(&server, &mut connection).await;
    home.script
        .push(answer("obsolete first answer", OVER_LIMIT));
    server
        .run_turn(&mut connection, &thread, "first user message")
        .await;
    connection.passed.clear();

    home.script.push(Step::Fail);
    server
        .turn_start(&mut connection, &thread, "prompt kept on failure")
        .await;
    let ended = connection.notification("turn/completed").await;
    assert_eq!(ended["turn"]["status"], "failed");
    assert_eq!(home.script.requests.lock().unwrap().len(), 2);
    assert!(logged(&post).is_empty());

    home.script.push(Step::Respond(summary()));
    home.script.answer("answered");
    server
        .run_turn(&mut connection, &thread, "next user message")
        .await;
    let requests = home.script.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 4);
    assert!(is_summary_request(&requests[2]));
    assert!(request_text(&requests[2]).contains("prompt kept on failure"));
    let input = request_text(&requests[3]);
    assert!(input.contains("prompt kept on failure"), "{input}");
    assert!(input.contains(SUMMARY_SENTINEL), "{input}");
}

/// A `PreCompact` stop before a turn records its prompt aborts the turn without a summary request
/// and keeps the prompt.
#[tokio::test]
async fn a_pre_turn_pre_compact_stop_aborts_the_turn_and_keeps_its_prompt() {
    let home = HooksHome::new();
    write_config(
        &home,
        &[matched(
            "PreCompact",
            "auto",
            r#"printf '%s' '{"continue":false,"stopReason":"stop compaction"}'"#,
        )],
    );
    let server = home.server();
    let mut connection = server.connect(1).await;
    let thread = start(&server, &mut connection).await;
    home.script.push(answer("first answer", OVER_LIMIT));
    server
        .run_turn(&mut connection, &thread, "first user message")
        .await;

    server
        .turn_start(&mut connection, &thread, "prompt kept on stop")
        .await;
    let ended = connection.notification("turn/completed").await;
    assert_eq!(ended["turn"]["status"], "interrupted");
    assert_eq!(home.script.requests.lock().unwrap().len(), 1);

    write_config(&home, &[]);
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
            .contains("prompt kept on stop")
    );
}

/// Ports `auto_compact_runs_after_resume_when_token_usage_is_over_limit`: one over-limit answer does
/// not compact until the next prompt, and the thread rebuilt from its rollout still knows the
/// reported total, so its first turn compacts before sampling.
#[tokio::test]
async fn a_resumed_thread_over_the_limit_compacts_before_its_first_turn() {
    let rollout_dir = tempfile::tempdir().unwrap();
    let mut home = HooksHome::new();
    home.store = Arc::new(RolloutThreadDirectory::new(rollout_dir.path()));
    write_config(&home, &[]);
    let server = home.server();
    let mut connection = server.connect(1).await;
    let thread = start(&server, &mut connection).await;
    home.script
        .push(answer("obsolete first answer", OVER_LIMIT));
    server
        .run_turn(&mut connection, &thread, "first user message")
        .await;
    assert_eq!(home.script.requests.lock().unwrap().len(), 1);

    super::super::unload(&server, &mut connection, &thread).await;
    server
        .call(
            &mut connection,
            "thread/resume",
            json!({"threadId": thread}),
        )
        .await;
    server
        .call(
            &mut connection,
            "thread/settings/update",
            json!({"threadId": thread, "model": MODEL}),
        )
        .await;
    home.script.push(Step::Respond(summary()));
    home.script.answer("after resume");
    server
        .run_turn(&mut connection, &thread, "after resume user message")
        .await;
    let requests = home.script.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 3);
    assert!(is_summary_request(&requests[1]));
    let input = request_text(&requests[2]);
    assert!(input.contains("after resume user message"), "{input}");
    assert!(input.contains(SUMMARY_SENTINEL), "{input}");
    assert!(!input.contains("obsolete first answer"), "{input}");
}

/// A `PostCompact` stop ends the turn after the replacement was committed, and the live thread
/// continues from that replacement as the rebuilt one does: Codex's `TurnAborted` after
/// `replace_compacted_history`. Covers the pre-turn check and a manual compaction.
#[tokio::test]
async fn a_post_compact_stop_keeps_the_replacement_in_the_live_thread() {
    for manual in [false, true] {
        let home = HooksHome::new();
        write_config(
            &home,
            &[matched(
                "PostCompact",
                if manual { "manual" } else { "auto" },
                r#"printf '%s' '{"continue":false,"stopReason":"stop after compaction"}'"#,
            )],
        );
        let server = home.server();
        let mut connection = server.connect(1).await;
        let thread = start(&server, &mut connection).await;
        home.script.push(answer(
            "obsolete first answer",
            if manual { 1_000 } else { OVER_LIMIT },
        ));
        server
            .run_turn(&mut connection, &thread, "first user message")
            .await;

        home.script.push(Step::Respond(summary()));
        if manual {
            let reply = server
                .call(
                    &mut connection,
                    "thread/compact/start",
                    json!({"threadId": thread}),
                )
                .await;
            assert_eq!(reply, json!({}));
        } else {
            server
                .turn_start(&mut connection, &thread, "stopped after compaction")
                .await;
        }
        let ended = connection.notification("turn/completed").await;
        assert_eq!(ended["turn"]["status"], "interrupted", "manual: {manual}");
        assert_eq!(home.script.requests.lock().unwrap().len(), 2);

        home.script.answer("continued");
        server
            .run_turn(&mut connection, &thread, "continue live")
            .await;
        let input = request_text(&home.script.last_request());
        assert!(
            input.contains(SUMMARY_SENTINEL),
            "manual: {manual}: {input}"
        );
        assert!(
            !input.contains("obsolete first answer"),
            "manual: {manual}: {input}"
        );
        assert!(
            input.contains("first user message"),
            "manual: {manual}: {input}"
        );
    }
}

/// The rollout files under `dir`, as JSON lines.
fn rollout_lines(dir: &std::path::Path) -> Vec<Value> {
    let mut lines = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(path) = pending.pop() {
        for entry in std::fs::read_dir(&path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path);
            } else if path
                .extension()
                .is_some_and(|extension| extension == "jsonl")
            {
                lines.extend(
                    std::fs::read_to_string(&path)
                        .unwrap()
                        .lines()
                        .filter_map(|line| serde_json::from_str(line).ok()),
                );
            }
        }
    }
    lines
}

/// Codex persists `TurnStarted` before the compaction that precedes a turn's prompt, so a fork
/// ending with the turn before, or before that turn, leaves the replacement out: the fork's first
/// request carries the first turn's own answer and none of the second turn's summary.
#[tokio::test]
async fn a_fork_before_a_compacted_turn_leaves_its_replacement_out() {
    let rollout_dir = tempfile::tempdir().unwrap();
    let mut home = HooksHome::new();
    home.store = Arc::new(RolloutThreadDirectory::new(rollout_dir.path()));
    write_config(&home, &[]);
    let server = home.server();
    let mut connection = server.connect(1).await;
    let thread = start(&server, &mut connection).await;
    home.script.push(answer("first turn answer", OVER_LIMIT));
    let first = server
        .run_turn(&mut connection, &thread, "first user message")
        .await;
    home.script.push(Step::Respond(summary()));
    home.script.answer("second turn answer");
    let second = server
        .run_turn(&mut connection, &thread, "second user message")
        .await;
    assert_eq!(home.script.requests.lock().unwrap().len(), 3);

    for params in [
        json!({"threadId": thread, "lastTurnId": first}),
        json!({"threadId": thread, "beforeTurnId": second}),
    ] {
        let forked = server
            .call(&mut connection, "thread/fork", params.clone())
            .await;
        let fork = forked["thread"]["id"].as_str().unwrap().to_owned();
        let turns = forked["thread"]["turns"].as_array().unwrap();
        assert_eq!(turns.len(), 1, "{params}");
        assert!(
            !turns[0].to_string().contains("contextCompaction"),
            "{params}"
        );
        home.script.answer("fork answer");
        server
            .run_turn(&mut connection, &fork, "fork user message")
            .await;
        let input = request_text(&home.script.last_request());
        assert!(input.contains("first turn answer"), "{params}: {input}");
        assert!(!input.contains(SUMMARY_SENTINEL), "{params}: {input}");
        assert!(!input.contains("second user message"), "{params}: {input}");
    }
}

/// A summary request is billed even when its summary is unusable. The live thread keeps checking
/// the sampling total, and so does the thread rebuilt from its rollout: its first turn compacts.
#[tokio::test]
async fn a_failed_summary_does_not_replace_the_sampling_total_a_resumed_thread_checks() {
    let rollout_dir = tempfile::tempdir().unwrap();
    let mut home = HooksHome::new();
    home.store = Arc::new(RolloutThreadDirectory::new(rollout_dir.path()));
    write_config(&home, &[]);
    let server = home.server();
    let mut connection = server.connect(1).await;
    let thread = start(&server, &mut connection).await;
    home.script
        .push(answer("obsolete first answer", OVER_LIMIT));
    server
        .run_turn(&mut connection, &thread, "first user message")
        .await;
    home.script.push(answer("", 1_000));
    server
        .turn_start(&mut connection, &thread, "second user message")
        .await;
    let ended = connection.notification("turn/completed").await;
    assert_eq!(ended["turn"]["status"], "failed");

    super::super::unload(&server, &mut connection, &thread).await;
    server
        .call(
            &mut connection,
            "thread/resume",
            json!({"threadId": thread}),
        )
        .await;
    server
        .call(
            &mut connection,
            "thread/settings/update",
            json!({"threadId": thread, "model": MODEL}),
        )
        .await;
    home.script.push(Step::Respond(summary()));
    home.script.answer("after resume");
    server
        .run_turn(&mut connection, &thread, "after resume user message")
        .await;
    let requests = home.script.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 4);
    assert!(is_summary_request(&requests[2]));
    let input = request_text(&requests[3]);
    assert!(input.contains(SUMMARY_SENTINEL), "{input}");
    assert!(!input.contains("obsolete first answer"), "{input}");
}

/// Ports the terminal half of `incomplete_response_during_compaction_is_terminal_and_counted`: a
/// summary response stopped at its output limit ends the compaction at once — no shorter request,
/// no retry — where an input that overflows the window is resent without its oldest item.
#[tokio::test]
async fn an_incomplete_summary_is_terminal_and_an_overflowing_one_is_trimmed() {
    for (failure, requests) in [
        (ra_core::error::ProviderErrorKind::OutputLimit, 2),
        (ra_core::error::ProviderErrorKind::ContextOverflow, 4),
    ] {
        let home = HooksHome::new();
        write_config(&home, &[]);
        let server = home.server();
        let mut connection = server.connect(1).await;
        let thread = start(&server, &mut connection).await;
        home.script.push(answer("first answer", OVER_LIMIT));
        server
            .run_turn(&mut connection, &thread, "first user message")
            .await;
        connection.passed.clear();
        home.script.push(Step::FailWith(failure));
        home.script.push(Step::Respond(summary()));
        home.script.answer("answered");
        server
            .turn_start(&mut connection, &thread, "second user message")
            .await;
        let ended = connection.notification("turn/completed").await;
        let all = home.script.requests.lock().unwrap().clone();
        assert_eq!(all.len(), requests, "{failure:?}");
        let events: Vec<_> = connection.passed.drain(..).collect();
        let retried = events
            .iter()
            .any(|event| event.to_string().contains("Reconnecting"));
        assert!(!retried, "{failure:?}");
        if failure == ra_core::error::ProviderErrorKind::OutputLimit {
            assert_eq!(ended["turn"]["status"], "failed");
        } else {
            assert_eq!(ended["turn"]["status"], "completed");
            let first = request_text(&all[1]);
            let second = request_text(&all[2]);
            assert!(first.contains("first user message"), "{first}");
            assert!(!second.contains("first user message"), "{second}");
        }
    }
}

/// The coding host retries a compaction request that failed transiently, as Codex retries up to
/// the provider's `stream_max_retries`, and reports each retry as a stream error.
#[tokio::test]
async fn a_transient_summary_failure_is_retried_by_default() {
    let home = HooksHome::new();
    write_config(&home, &[]);
    let server = home.server();
    let mut connection = server.connect(1).await;
    let thread = start(&server, &mut connection).await;
    home.script
        .push(answer("obsolete first answer", OVER_LIMIT));
    server
        .run_turn(&mut connection, &thread, "first user message")
        .await;
    connection.passed.clear();
    home.script.push(Step::FailWith(
        ra_core::error::ProviderErrorKind::ServerError,
    ));
    home.script.push(Step::Respond(summary()));
    home.script.answer("answered");
    server
        .run_turn(&mut connection, &thread, "second user message")
        .await;
    let requests = home.script.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 4);
    assert!(is_summary_request(&requests[1]) && is_summary_request(&requests[2]));
    let events: Vec<_> = connection.passed.drain(..).collect();
    assert!(
        events
            .iter()
            .any(|event| event.to_string().contains("Reconnecting... 1/5")),
        "{events:?}"
    );
    assert!(request_text(&requests[3]).contains(SUMMARY_SENTINEL));
}

/// What a mid-turn summary costs is recorded once, with the run's own spend, and marked as a
/// summary's — not as sampling — so a rebuilt thread does not read it as the history's size.
#[tokio::test]
async fn a_mid_turn_summary_is_recorded_once_as_a_summary() {
    let rollout_dir = tempfile::tempdir().unwrap();
    let mut home = HooksHome::new();
    home.store = Arc::new(RolloutThreadDirectory::new(rollout_dir.path()));
    write_config(&home, &[]);
    let server = home.server();
    let mut connection = server.connect(1).await;
    let thread = start(&server, &mut connection).await;
    home.script.push(echo_call("one", OVER_LIMIT));
    home.script.push(Step::Respond(reporting(summary(), 7_777)));
    home.script.push(echo_call("two", OVER_LIMIT));
    home.script.push(Step::Respond(reporting(summary(), 7_777)));
    home.script.push(answer("final answer", 1_000));
    server
        .run_turn(&mut connection, &thread, "first user message")
        .await;
    super::super::unload(&server, &mut connection, &thread).await;

    let usage: Vec<Value> = rollout_lines(rollout_dir.path())
        .into_iter()
        .filter(|line| {
            line.to_string().contains("\"total_tokens\":7777") || line.to_string().contains("7677")
        })
        .collect();
    assert_eq!(usage.len(), 2, "{usage:?}");
    assert!(
        usage
            .iter()
            .all(|line| !line.to_string().contains("\"sampling\":true")),
        "{usage:?}"
    );
}

/// While a turn is still compacting ahead of its start — its `PostCompact` hook not yet answered —
/// the rollout holds the replacement but not the turn's start. A fork ending with the turn before
/// still leaves the replacement out, as Codex's persisted turn start makes it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fork_during_a_pre_turn_compaction_leaves_its_replacement_out() {
    let rollout_dir = tempfile::tempdir().unwrap();
    let mut home = HooksHome::new();
    home.store = Arc::new(RolloutThreadDirectory::new(rollout_dir.path()));
    let (entered, release) = (home.path("entered"), home.path("release"));
    write_config(
        &home,
        &[matched(
            "PostCompact",
            "auto",
            &format!(
                "cat > /dev/null; touch '{}'; while [ ! -f '{}' ]; do sleep 0.05; done",
                entered.display(),
                release.display()
            ),
        )],
    );
    let server = home.server();
    let mut connection = server.connect(1).await;
    let thread = start(&server, &mut connection).await;
    home.script.push(answer("first turn answer", OVER_LIMIT));
    let first = server
        .run_turn(&mut connection, &thread, "first user message")
        .await;
    home.script.push(Step::Respond(summary()));
    home.script.answer("second turn answer");
    server
        .turn_start(&mut connection, &thread, "second user message")
        .await;
    timeout(PATIENCE, async {
        while !entered.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the PostCompact hook runs");
    // A settings update recorded while the turn is still compacting lands after its replacement.
    server
        .call(
            &mut connection,
            "thread/settings/update",
            json!({"threadId": thread, "model": "later-model"}),
        )
        .await;
    // Persisted before it is announced, so the fork below reads a rollout that holds it.
    loop {
        let updated = connection.notification("thread/settings/updated").await;
        if updated["threadSettings"]["model"] == "later-model" {
            break;
        }
    }

    let mut forking = server.connect(2).await;
    let forked = server
        .call(
            &mut forking,
            "thread/fork",
            json!({"threadId": thread, "lastTurnId": first}),
        )
        .await;
    let fork = forked["thread"]["id"].as_str().unwrap().to_owned();
    assert!(
        !forked["thread"]["turns"]
            .to_string()
            .contains("contextCompaction")
    );
    std::fs::write(&release, "").unwrap();
    connection.notification("turn/completed").await;

    home.script.answer("fork answer");
    server
        .run_turn(&mut forking, &fork, "fork user message")
        .await;
    let input = request_text(&home.script.last_request());
    assert!(input.contains("first turn answer"), "{input}");
    assert!(!input.contains(SUMMARY_SENTINEL), "{input}");
}

/// A sampling that reported no usage leaves no total to check, in the live thread and in the one
/// rebuilt from its rollout alike. Both turns run on a model whose window is unknown, so neither
/// compacts; once the thread moves to a known window, neither path compacts on the first turn's
/// older over-limit figure.
#[tokio::test]
async fn a_sampling_without_usage_clears_the_total_live_and_after_resume() {
    for resume in [false, true] {
        let rollout_dir = tempfile::tempdir().unwrap();
        let mut home = HooksHome::new();
        home.store = Arc::new(RolloutThreadDirectory::new(rollout_dir.path()));
        write_config(&home, &[]);
        let server = home.server();
        let mut connection = server.connect(1).await;
        let thread = start_thread(&server, &mut connection).await;
        home.script.push(answer("first answer", OVER_LIMIT));
        server
            .run_turn(&mut connection, &thread, "first user message")
            .await;
        home.script.answer("second answer without usage");
        server
            .run_turn(&mut connection, &thread, "second user message")
            .await;
        if resume {
            super::super::unload(&server, &mut connection, &thread).await;
            server
                .call(
                    &mut connection,
                    "thread/resume",
                    json!({"threadId": thread}),
                )
                .await;
        }
        server
            .call(
                &mut connection,
                "thread/settings/update",
                json!({"threadId": thread, "model": MODEL}),
            )
            .await;
        home.script.answer("third answer");
        server
            .run_turn(&mut connection, &thread, "third user message")
            .await;
        let requests = home.script.requests.lock().unwrap().clone();
        assert_eq!(requests.len(), 3, "resume: {resume}");
        assert!(!is_summary_request(&requests[2]), "resume: {resume}");
    }
}
