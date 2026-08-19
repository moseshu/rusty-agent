//! Integration tests for ExecSessionId and ExecSessionState state machine.

use ra_exec::{
    output::ExecEvictionReason,
    session::{ExecSessionId, ExecSessionState, ExecSessionTransitionError},
};

#[test]
fn test_session_id_generation_and_display() {
    let id1 = ExecSessionId::new("exec-custom-1");
    assert_eq!(id1.as_str(), "exec-custom-1");
    assert_eq!(format!("{id1}"), "exec-custom-1");

    let id2 = ExecSessionId::generate();
    assert!(id2.as_str().starts_with("exec-"));

    let json_str = serde_json::to_string(&id1).expect("must serialize");
    assert_eq!(json_str, "\"exec-custom-1\"");
    let restored: ExecSessionId = serde_json::from_str(&json_str).expect("must deserialize");
    assert_eq!(id1, restored);

    let res_id =
        ra_exec::session::session_resource_id(&id1).expect("must derive session ResourceId");
    assert_eq!(res_id.to_string(), "process:exec-custom-1");
}

#[test]
fn test_session_state_valid_lifecycle_transitions() {
    // Standard run: Reserved -> Starting -> Running -> Exited
    let mut state = ExecSessionState::Reserved;
    assert!(state.is_active());
    assert!(!state.is_terminal());

    assert!(state.can_transition_to(&ExecSessionState::Starting));
    state
        .transition_to(ExecSessionState::Starting)
        .expect("must transition to Starting");
    assert!(state.is_active());
    assert!(!state.is_terminal());

    assert!(state.can_transition_to(&ExecSessionState::Running));
    state
        .transition_to(ExecSessionState::Running)
        .expect("must transition to Running");
    assert!(state.is_active());
    assert!(!state.is_terminal());

    let exit_state = ExecSessionState::Exited { exit_code: Some(0) };
    assert!(state.can_transition_to(&exit_state));
    state
        .transition_to(exit_state.clone())
        .expect("must transition to Exited");
    assert!(!state.is_active());
    assert!(state.is_terminal());

    // Aborted before spawn: Reserved -> Cancelled
    let mut state = ExecSessionState::Reserved;
    state
        .transition_to(ExecSessionState::Cancelled)
        .expect("must cancel from reserved");
    assert!(state.is_terminal());

    // Spawn failure: Starting -> Failed
    let mut state = ExecSessionState::Starting;
    state
        .transition_to(ExecSessionState::Failed {
            error: "binary not found".into(),
        })
        .expect("must fail from starting");
    assert!(state.is_terminal());

    // Evicted during run: Running -> Expired
    let mut state = ExecSessionState::Running;
    state
        .transition_to(ExecSessionState::Expired {
            reason: ExecEvictionReason::IdleTimeout,
        })
        .expect("must expire from running");
    assert!(state.is_terminal());
}

#[test]
fn test_session_state_rejects_illegal_and_retrograde_transitions() {
    let mut state = ExecSessionState::Exited { exit_code: Some(0) };

    // Terminal states cannot transition to anything
    assert_eq!(
        state.transition_to(ExecSessionState::Running),
        Err(ExecSessionTransitionError::new(
            ExecSessionState::Exited { exit_code: Some(0) },
            ExecSessionState::Running,
        ))
    );

    let mut state = ExecSessionState::Expired {
        reason: ExecEvictionReason::CapacityExceeded,
    };
    assert!(!state.can_transition_to(&ExecSessionState::Starting));
    assert!(state.transition_to(ExecSessionState::Starting).is_err());

    // Cannot jump directly from Reserved to Exited or Expired
    let mut state = ExecSessionState::Reserved;
    assert!(!state.can_transition_to(&ExecSessionState::Exited { exit_code: None }));
    assert!(
        state
            .transition_to(ExecSessionState::Exited { exit_code: None })
            .is_err()
    );
}

#[test]
fn test_session_state_serialization_roundtrip() {
    let states = vec![
        ExecSessionState::Reserved,
        ExecSessionState::Starting,
        ExecSessionState::Running,
        ExecSessionState::Exited { exit_code: Some(1) },
        ExecSessionState::Exited { exit_code: None },
        ExecSessionState::Failed {
            error: "network timeout".into(),
        },
        ExecSessionState::Cancelled,
        ExecSessionState::Expired {
            reason: ExecEvictionReason::IdleTimeout,
        },
    ];

    for state in states {
        let serialized = serde_json::to_string(&state).expect("must serialize");
        let restored: ExecSessionState =
            serde_json::from_str(&serialized).expect("must deserialize");
        assert_eq!(state, restored);
    }
}
