//! `ra-sandbox::unix_local`: the argument vector a request turns into.
//!
//! The rest of the exec tests read the answer out of the command itself, which is the better test
//! wherever it is possible. It is not possible here: a request that names another account becomes a
//! `sudo` invocation, and running one needs a second account with passwordless `sudo` — a property
//! of the machine the tests happen to run on, not of this code. So the shaping is asserted
//! directly, and what the shaped command *does* is covered where it can be run.

use ra_core::sandbox::{ExecRequest, ShellInvocation, User};
use ra_sandbox::unix_local::prepare_exec_command;

#[test]
fn the_default_shell_prefix_is_sh_dash_c() {
    let shaped = prepare_exec_command(&ExecRequest::new(["ls -la".to_owned()]));

    // Not `sh -lc`, which is the protocol's default and what a remote backend uses. A local command
    // that sourced the developer's login profile would behave differently on every machine.
    assert_eq!(shaped, vec!["sh", "-c", "ls -la"]);
}

#[test]
fn several_arguments_are_quoted_back_into_one_command_line() {
    let shaped = prepare_exec_command(&ExecRequest::new([
        "grep".to_owned(),
        "-n".to_owned(),
        "two words".to_owned(),
        "it's".to_owned(),
    ]));

    // A single argument is already a command line and is passed through; several are quoted, and an
    // embedded quote leaves the quoted run rather than ending it.
    assert_eq!(
        shaped,
        vec!["sh", "-c", r#"grep -n 'two words' 'it'"'"'s'"#]
    );
}

#[test]
fn a_request_with_no_shell_is_the_argument_vector_it_was_given() {
    let shaped = prepare_exec_command(
        &ExecRequest::new(["printf".to_owned(), "%s".to_owned(), "$HOME".to_owned()])
            .with_shell(ShellInvocation::None),
    );

    assert_eq!(shaped, vec!["printf", "%s", "$HOME"]);
}

#[test]
fn a_custom_prefix_replaces_the_default_one_and_an_empty_prefix_replaces_the_shell() {
    let with_prefix = prepare_exec_command(&ExecRequest::new(["echo hi".to_owned()]).with_shell(
        ShellInvocation::Prefix(vec!["bash".to_owned(), "-lc".to_owned()]),
    ));
    assert_eq!(with_prefix, vec!["bash", "-lc", "echo hi"]);

    // An empty prefix is no shell at all, which is how the reference reads an empty list.
    let without = prepare_exec_command(
        &ExecRequest::new(["echo".to_owned(), "hi".to_owned()])
            .with_shell(ShellInvocation::Prefix(Vec::new())),
    );
    assert_eq!(without, vec!["echo", "hi"]);
}

#[test]
fn naming_an_account_puts_sudo_in_front_of_the_whole_command() {
    let shaped = prepare_exec_command(
        &ExecRequest::new([
            "sh".to_owned(),
            "-lc".to_owned(),
            "[ -r \"$1\" ]".to_owned(),
        ])
        .with_shell(ShellInvocation::None)
        .as_user(User::new("sandbox-user")),
    );

    // `--` closes sudo's own options, so a command whose first argument starts with a dash is not
    // read as one of them.
    assert_eq!(
        shaped,
        vec![
            "sudo",
            "-u",
            "sandbox-user",
            "--",
            "sh",
            "-lc",
            "[ -r \"$1\" ]"
        ]
    );
}

#[test]
fn the_account_wraps_the_shell_rather_than_the_other_way_round() {
    let shaped = prepare_exec_command(
        &ExecRequest::new(["whoami".to_owned()]).as_user(User::new("sandbox-user")),
    );

    // `sudo` outside, shell inside: the other order would run `sudo` through a shell owned by the
    // account that is supposed to be switched away from.
    assert_eq!(
        shaped,
        vec!["sudo", "-u", "sandbox-user", "--", "sh", "-c", "whoami"]
    );
}
