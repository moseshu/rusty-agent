//! Ported from the tests of Codex's `shell-command/src/command_safety/is_dangerous_command.rs`;
//! its PowerShell case is not ported.

use ra_coding::shell_command::{DangerousCommandMatch, dangerous_command_match};

/// Codex's `MAX_DANGEROUS_COMMAND_WRAPPER_DEPTH`.
const MAX_WRAPPER_DEPTH: usize = 8;

fn vec_str(items: &[&str]) -> Vec<String> {
    items.iter().map(std::string::ToString::to_string).collect()
}

#[test]
fn rm_rf_is_dangerous() {
    assert_eq!(
        dangerous_command_match(&vec_str(&["rm", "-rf", "/"])),
        Some(DangerousCommandMatch::ForcedRm)
    );
}

#[test]
fn rm_f_is_dangerous() {
    assert_eq!(
        dangerous_command_match(&vec_str(&["rm", "-f", "/"])),
        Some(DangerousCommandMatch::ForcedRm)
    );
}

#[test]
fn forced_rm_variants_are_dangerous() {
    for command in [
        vec_str(&["/bin/rm", "-fr", "/tmp/example"]),
        vec_str(&["rm", "-r", "-f", "/tmp/example"]),
        vec_str(&["rm", "--force", "/tmp/example"]),
        vec_str(&["rm", "/tmp/example", "-f"]),
        vec_str(&["sudo", "rm", "-rf", "/tmp/example"]),
        vec_str(&["env", "TARGET=/tmp/example", "rm", "-rf", "/tmp/example"]),
    ] {
        assert_eq!(
            dangerous_command_match(&command),
            Some(DangerousCommandMatch::ForcedRm),
            "{command:?}"
        );
    }
}

#[test]
fn deeply_nested_command_wrappers_fail_closed() {
    for (depth, expected) in [
        (MAX_WRAPPER_DEPTH, DangerousCommandMatch::ForcedRm),
        (MAX_WRAPPER_DEPTH + 1, DangerousCommandMatch::Other),
    ] {
        let command = std::iter::repeat_n("env", depth)
            .chain(["rm", "-rf", "/tmp/example"])
            .map(str::to_owned)
            .collect::<Vec<_>>();

        assert_eq!(dangerous_command_match(&command), Some(expected));
    }
}

#[test]
fn forced_rm_in_complex_shell_syntax_is_dangerous() {
    for script in [
        "printf x | rm -rf /tmp/example",
        "if test -d /tmp/example; then rm --force /tmp/example; fi",
        "rm -rf \"$TARGET\" >/dev/null",
        "for target in /tmp/a /tmp/b; do rm -r -f \"$target\"; done",
        "echo \"$(rm -rf /tmp/example)\"",
        "bash -c 'rm -rf /tmp/example'",
        "trap 'rm -rf /tmp/example' EXIT",
        "for a in '-C5a25KeRr' '--' '--json' '--bogus'; do HOME=$(mktemp -d) MDE_URL=http://127.0.0.1:1 MDE_TOKEN=x node cli/mde.cjs ls \"$a\" >/tmp/mde-review-out 2>/tmp/mde-review-err; code=$?; printf '%s\\t%s\\t%s\\n' \"$a\" \"$code\" \"$(tr '\\n' ' ' </tmp/mde-review-err)\"; rm -rf \"$HOME\"; done",
    ] {
        let command = vec_str(&["bash", "-lc", script]);
        assert_eq!(
            dangerous_command_match(&command),
            Some(DangerousCommandMatch::ForcedRm),
            "{script}"
        );
    }
}

#[test]
fn non_forced_or_non_literal_rm_is_not_dangerous() {
    for command in [
        vec_str(&["rm", "-r", "/tmp/example"]),
        vec_str(&["rm", "--", "-f"]),
        vec_str(&["bash", "-lc", "echo 'rm -rf /tmp/example'"]),
        vec_str(&["bash", "-lc", "cmd=rm; $cmd -rf /tmp/example"]),
        vec_str(&["bash", "-lc", "if then rm -rf /tmp/example"]),
        vec_str(&["env", "TARGET=/tmp/example", "rm", "-r", "/tmp/example"]),
        vec_str(&["bash", "-lc", "trap 'echo rm -rf /tmp/example' EXIT"]),
    ] {
        assert_eq!(dangerous_command_match(&command), None, "{command:?}");
    }
}
