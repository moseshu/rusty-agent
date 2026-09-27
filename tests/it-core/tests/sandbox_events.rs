//! `ra-core::sandbox::events`: the audit event model, the payload policy and the JSONL line format.
//!
//! The reference has no test file of its own for `session/events.py` or the event half of
//! `session/utils.py`; their behaviour is pinned through `test_session_sinks.py` and
//! `test_session_manager.py`, whose delivery tests live in `it-sandbox/session_instrumentation`.
//! What is here is the part of those tests that belongs to the model: what a policy merge answers,
//! what a sink's copy of an event keeps, and what a line looks like byte for byte.

use std::time::{Duration, UNIX_EPOCH};

use ra_core::sandbox::{
    ErrorCategory, ErrorCode, EventPayloadPolicy, EventPhase, OpName, SandboxError,
    SandboxSessionEvent, SandboxSessionEventBase, SandboxSessionFinishEvent,
    SandboxSessionStartEvent, event_to_json_line, format_event_timestamp, parse_event_timestamp,
    safe_decode, validate_sandbox_session_event,
};
use serde_json::{Value, json};
use uuid::Uuid;

fn finish_with_output(stdout: &[u8], stderr: &[u8]) -> SandboxSessionEvent {
    SandboxSessionFinishEvent::new(Uuid::new_v4(), 1, OpName::Exec, "span_exec", true, 0.0)
        .with_output(Some(stdout.to_vec()), Some(stderr.to_vec()))
        .into()
}

// --- the policy -----------------------------------------------------------------------------------

#[test]
fn a_policy_that_sets_nothing_answers_with_the_reference_defaults() {
    let policy = EventPayloadPolicy::new();
    assert!(!policy.include_exec_output());
    assert_eq!(policy.max_stdout_chars(), 8_000);
    assert_eq!(policy.max_stderr_chars(), 8_000);
    assert!(policy.include_write_len());
}

#[test]
fn an_override_replaces_only_what_it_set() {
    let base = EventPayloadPolicy::new()
        .with_include_exec_output(true)
        .with_max_stdout_chars(10);
    let merged = base.overridden_by(&EventPayloadPolicy::new().with_max_stderr_chars(3));

    assert!(merged.include_exec_output(), "unset in the override, kept");
    assert_eq!(merged.max_stdout_chars(), 10);
    assert_eq!(merged.max_stderr_chars(), 3);

    let turned_off =
        merged.overridden_by(&EventPayloadPolicy::new().with_include_exec_output(false));
    assert!(
        !turned_off.include_exec_output(),
        "set in the override, replaced"
    );
}

#[test]
fn an_override_set_to_the_default_value_still_overrides() {
    // The distinction the reference draws with its set-field tracking: a later policy that says
    // "false" out loud is not the same as one that says nothing.
    let base = EventPayloadPolicy::new().with_include_write_len(false);
    let merged = base.overridden_by(&EventPayloadPolicy::new().with_include_write_len(true));
    assert!(merged.include_write_len());
}

// --- what a sink's copy keeps ---------------------------------------------------------------------

/// `test_session_manager.py::test_instrumentation_redacts_raw_exec_bytes_when_output_disabled`, the
/// half that is the model's.
#[test]
fn output_left_out_by_the_policy_is_dropped_raw_bytes_and_all() {
    let event = finish_with_output(b"secret", b"secret2");
    let copy = event.with_policy_applied(&EventPayloadPolicy::new());
    let finish = copy.as_finish().expect("finish");
    assert_eq!(finish.stdout(), None);
    assert_eq!(finish.stderr(), None);
    assert_eq!(finish.stdout_bytes(), None);
    assert_eq!(finish.stderr_bytes(), None);

    // The original is untouched: another sink may be entitled to it.
    assert_eq!(
        event.as_finish().expect("finish").stdout_bytes(),
        Some(&b"secret"[..])
    );
}

#[test]
fn output_included_by_the_policy_is_decoded_and_truncated_per_stream() {
    let event = finish_with_output(b"abcdef", b"uvwxyz");
    let policy = EventPayloadPolicy::new()
        .with_include_exec_output(true)
        .with_max_stdout_chars(3)
        .with_max_stderr_chars(10);
    let copy = event.with_policy_applied(&policy);
    let finish = copy.as_finish().expect("finish");
    assert_eq!(finish.stdout(), Some("abc…"));
    assert_eq!(finish.stderr(), Some("uvwxyz"));
}

#[test]
fn the_write_length_is_dropped_only_when_the_policy_says_so() {
    let mut start = SandboxSessionStartEvent::new(Uuid::new_v4(), 1, OpName::Write, "span");
    start
        .base_mut()
        .data_mut()
        .insert("path".to_owned(), json!("x.txt"));
    start
        .base_mut()
        .data_mut()
        .insert("bytes".to_owned(), json!(5));
    let event = SandboxSessionEvent::from(start);

    let kept = event.with_policy_applied(&EventPayloadPolicy::new());
    assert_eq!(kept.data().get("bytes"), Some(&json!(5)));

    let dropped =
        event.with_policy_applied(&EventPayloadPolicy::new().with_include_write_len(false));
    assert_eq!(dropped.data().get("bytes"), None);
    assert_eq!(dropped.data().get("path"), Some(&json!("x.txt")));
}

#[test]
fn decoding_replaces_invalid_utf8_and_counts_characters_not_bytes() {
    assert_eq!(safe_decode(b"ok\xff", 10), "ok\u{fffd}");
    assert_eq!(safe_decode("héllo".as_bytes(), 2), "hé…");
    assert_eq!(
        safe_decode(b"abc", 3),
        "abc",
        "exactly at the limit is not cut"
    );
    assert_eq!(safe_decode(b"abc", 0), "…");
}

// --- the line format ------------------------------------------------------------------------------

#[test]
fn a_line_is_sorted_compact_ascii_and_ends_with_a_newline() {
    let session_id = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").expect("uuid");
    let mut data = serde_json::Map::new();
    data.insert("path".to_owned(), json!("é/\u{1f600}\n"));
    let start = SandboxSessionStartEvent::from_base(
        SandboxSessionEventBase::new(session_id, 1, OpName::Write, "span_write")
            .with_event_id(Uuid::parse_str("00000000-0000-4000-8000-000000000001").expect("uuid"))
            .with_ts(UNIX_EPOCH + Duration::from_micros(1_790_000_000_123_456))
            .with_data(data),
    );

    let line = event_to_json_line(&start.into());

    assert_eq!(
        line,
        concat!(
            r#"{"data":{"path":"\u00e9/\ud83d\ude00\n"},"#,
            r#""event_id":"00000000-0000-4000-8000-000000000001","op":"write","#,
            r#""parent_span_id":null,"phase":"start","seq":1,"#,
            r#""session_id":"550e8400-e29b-41d4-a716-446655440000","span_id":"span_write","#,
            r#""trace_id":null,"ts":"2026-09-21T14:13:20.123456Z","version":1}"#,
            "\n"
        )
    );
}

#[test]
fn a_finish_line_carries_every_field_but_the_raw_output() {
    let event = finish_with_output(b"raw", b"raw");
    let line = event_to_json_line(&event);
    let value: Value = serde_json::from_str(&line).expect("json");
    let fields: Vec<&str> = value
        .as_object()
        .expect("object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        fields,
        [
            "data",
            "duration_ms",
            "error_code",
            "error_message",
            "error_retryable",
            "error_type",
            "event_id",
            "ok",
            "op",
            "parent_span_id",
            "phase",
            "seq",
            "session_id",
            "span_id",
            "stderr",
            "stdout",
            "trace_id",
            "ts",
            "version",
        ]
    );
    assert_eq!(value["duration_ms"], json!(0.0));
    assert!(!line.contains("stdout_bytes"));
}

#[test]
fn a_timestamp_is_written_as_the_reference_writes_a_utc_time() {
    assert_eq!(format_event_timestamp(UNIX_EPOCH), "1970-01-01T00:00:00Z");
    assert_eq!(
        format_event_timestamp(UNIX_EPOCH + Duration::from_micros(951_782_400_000_001)),
        "2000-02-29T00:00:00.000001Z"
    );
    // Below a microsecond is not written, as the reference cannot hold it.
    assert_eq!(
        format_event_timestamp(UNIX_EPOCH + Duration::from_nanos(999)),
        "1970-01-01T00:00:00Z"
    );
}

#[test]
fn a_timestamp_reads_back_from_either_offset_form_or_epoch_seconds() {
    let expected = UNIX_EPOCH + Duration::from_micros(1_790_000_000_123_456);
    for text in [
        "2026-09-21T14:13:20.123456Z",
        "2026-09-21T16:13:20.123456+02:00",
        "2026-09-21T14:13:20.123456000Z",
    ] {
        assert_eq!(parse_event_timestamp(&json!(text)), Ok(expected), "{text}");
    }
    assert_eq!(
        parse_event_timestamp(&json!(10)),
        Ok(UNIX_EPOCH + Duration::from_secs(10))
    );
    assert!(parse_event_timestamp(&json!("yesterday")).is_err());
    assert!(parse_event_timestamp(&json!("2026-13-01T00:00:00Z")).is_err());
}

// --- parsing --------------------------------------------------------------------------------------

#[test]
fn a_payload_is_parsed_into_the_phase_it_names() {
    let session_id = Uuid::new_v4();
    let start = validate_sandbox_session_event(json!({
        "session_id": session_id.to_string(),
        "seq": 1,
        "op": "exec",
        "phase": "start",
        "span_id": "s",
    }))
    .expect("start");
    assert_eq!(start.phase(), EventPhase::Start);
    // Fields the reference fills with defaults are filled here too.
    assert_eq!(start.base().version(), 1);
    assert!(start.data().is_empty());
    assert_eq!(start.base().trace_id(), None);

    let finish = validate_sandbox_session_event(json!({
        "session_id": session_id.to_string(),
        "seq": 2,
        "op": "exec",
        "phase": "finish",
        "span_id": "s",
        "ok": false,
        "duration_ms": 1.5,
        "error_code": "exec_timeout",
    }))
    .expect("finish");
    let finish = finish.as_finish().expect("finish");
    assert!(!finish.ok());
    assert_eq!(finish.error_code(), Some(ErrorCode::ExecTimeout));
}

#[test]
fn a_payload_without_a_known_phase_is_refused() {
    let base =
        json!({"session_id": Uuid::new_v4().to_string(), "seq": 1, "op": "exec", "span_id": "s"});
    let mut unknown = base.clone();
    unknown["phase"] = json!("middle");
    assert!(validate_sandbox_session_event(unknown).is_err());
    assert!(validate_sandbox_session_event(base).is_err());
}

#[test]
fn a_line_parses_back_into_the_same_event() {
    let event = SandboxSessionEvent::from(SandboxSessionFinishEvent::new(
        Uuid::new_v4(),
        4,
        OpName::Read,
        "span_read",
        false,
        2.5,
    ));
    let line = event_to_json_line(&event);
    let parsed =
        validate_sandbox_session_event(serde_json::from_str(&line).expect("json")).expect("parse");
    // The timestamp loses what is below a microsecond on the way; everything else survives.
    assert_eq!(parsed.event_id(), event.event_id());
    assert_eq!(parsed.seq(), 4);
    assert_eq!(parsed.op(), OpName::Read);
    assert_eq!(parsed.phase(), EventPhase::Finish);
}

// --- the code a failing sink travels in -----------------------------------------------------------

#[test]
fn a_sink_failure_has_a_code_of_its_own_that_the_reference_does_not_publish() {
    let error = SandboxError::event_sink_failed(OpName::Read, "sandbox event sink failed: X");
    assert_eq!(error.error_code(), ErrorCode::EventSinkFailed);
    assert_eq!(error.error_code().as_str(), "event_sink_failed");
    assert_eq!(error.error_code().reference_code(), None);
    assert_eq!(error.error_code().reference_type_name(), "RuntimeError");
    assert_eq!(error.category(), ErrorCategory::Runtime);
    assert_eq!(error.retryable(), None);
    assert_eq!(error.op(), OpName::Read);
}

#[test]
fn a_reference_code_is_its_own_reference_code_and_names_its_class() {
    assert_eq!(
        ErrorCode::WorkspaceReadNotFound.reference_code(),
        Some(ErrorCode::WorkspaceReadNotFound)
    );
    assert_eq!(
        ErrorCode::WorkspaceReadNotFound.reference_type_name(),
        "WorkspaceReadNotFoundError"
    );
    assert_eq!(
        ErrorCode::ExecNonzero.reference_type_name(),
        "ExecNonZeroError"
    );
    assert_eq!(
        ErrorCode::MountFailed.reference_type_name(),
        "MountCommandError"
    );
    assert_eq!(
        ErrorCode::MountMissingTool.reference_type_name(),
        "MountToolMissingError"
    );
}

#[test]
fn out_of_range_numeric_timestamps_are_parse_errors() {
    for seconds in [1e100, 1e19, -1.0] {
        assert!(parse_event_timestamp(&json!(seconds)).is_err());
        let mut payload = serde_json::to_value(finish_with_output(b"", b"")).unwrap();
        payload["ts"] = json!(seconds);
        assert!(validate_sandbox_session_event(payload).is_err());
    }
}

#[test]
fn concrete_events_preserve_and_validate_their_phase() {
    let start = SandboxSessionStartEvent::new(Uuid::new_v4(), 1, OpName::Start, "audit");
    let finish =
        SandboxSessionFinishEvent::new(Uuid::new_v4(), 2, OpName::Start, "audit", true, 1.0);
    let start_json = serde_json::to_value(&start).unwrap();
    let finish_json = serde_json::to_value(&finish).unwrap();
    assert_eq!(start_json["phase"], "start");
    assert_eq!(finish_json["phase"], "finish");
    assert_eq!(
        validate_sandbox_session_event(start_json.clone()).unwrap(),
        start.into()
    );
    assert_eq!(
        validate_sandbox_session_event(finish_json.clone()).unwrap(),
        finish.into()
    );
    assert!(serde_json::from_value::<SandboxSessionStartEvent>(finish_json.clone()).is_err());
    let mut wrong_finish = finish_json.clone();
    wrong_finish["phase"] = json!("start");
    assert!(serde_json::from_value::<SandboxSessionFinishEvent>(wrong_finish).is_err());
    let mut default_start = start_json;
    default_start.as_object_mut().unwrap().remove("phase");
    assert!(serde_json::from_value::<SandboxSessionStartEvent>(default_start.clone()).is_ok());
    assert!(validate_sandbox_session_event(default_start).is_err());
    let mut default_finish = finish_json;
    default_finish.as_object_mut().unwrap().remove("phase");
    assert!(serde_json::from_value::<SandboxSessionFinishEvent>(default_finish.clone()).is_ok());
    assert!(validate_sandbox_session_event(default_finish).is_err());
}
