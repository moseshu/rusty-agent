//! Upstream command-hook timeout defaults, limits and warning boundaries.

use ra_coding::hooks::timing::normalize_command_hook;
use ra_core::hook::HookEventName;
use std::path::Path;

#[test]
fn shutdown_events_default_to_one_second_and_clamp_at_three() {
    for (event, label) in [
        (HookEventName::SessionEnd, "SessionEnd"),
        (HookEventName::Interrupt, "Interrupt"),
    ] {
        for (timeout, expected) in [
            (None, 1),
            (Some(0), 1),
            (Some(1), 1),
            (Some(2), 2),
            (Some(3), 3),
            (Some(4), 3),
            (Some(u64::MAX), 3),
        ] {
            let mut warnings = Vec::new();
            assert_eq!(
                normalize_command_hook(event, timeout, Path::new("hooks.json"), &mut warnings),
                expected
            );
            let expected_warnings = if timeout.is_some_and(|value| value > 3) {
                vec![format!("clamping {label} hook timeout to 3s in hooks.json")]
            } else {
                Vec::new()
            };
            assert_eq!(warnings, expected_warnings);
        }
    }
}

#[test]
fn ordinary_events_keep_the_ten_minute_default_without_an_upper_cap() {
    for event in [
        HookEventName::PreToolUse,
        HookEventName::PermissionRequest,
        HookEventName::PostToolUse,
        HookEventName::Stop,
        HookEventName::SubagentStop,
    ] {
        let mut warnings = Vec::new();
        assert_eq!(
            normalize_command_hook(event, None, Path::new("hooks.json"), &mut warnings),
            600
        );
        assert_eq!(
            normalize_command_hook(event, Some(0), Path::new("hooks.json"), &mut warnings),
            1
        );
        assert_eq!(
            normalize_command_hook(
                event,
                Some(u64::MAX),
                Path::new("hooks.json"),
                &mut warnings
            ),
            u64::MAX
        );
        assert!(warnings.is_empty());
    }
}
